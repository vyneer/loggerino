#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::match_wild_err_arm)]
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]

use env_logger::Env;

use crossbeam_channel::{Receiver, Sender};
use log::{error, info};
use std::{env, panic, thread, time::Duration};

mod embeds;
mod logger;
mod phrases;
mod util;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let log_level = env::var("DEBUG")
        .ok()
        .map(|val| match val.as_str() {
            "0" | "false" | "" => "info",
            "1" | "true" => "debug",
            "2" | "trace" => "trace",
            _ => panic!("Please set the DEBUG env correctly."),
        })
        .unwrap();
    let params = format!(
        "host={} port={} user={} password={}",
        env::var("POSTGRES_HOST").unwrap().as_str(),
        env::var("POSTGRES_PORT").unwrap().as_str(),
        env::var("POSTGRES_USER").unwrap().as_str(),
        env::var("POSTGRES_PASSWORD").unwrap().as_str()
    );
    let grpc_status = env::var("GRPC_STATUS")
        .ok()
        .map(|val| match val.as_str() {
            "0" | "false" | "" => false,
            "1" | "true" => true,
            _ => panic!("Please set the GRPC_STATUS env correctly."),
        })
        .unwrap();
    let mut grpc_params = String::new();
    if grpc_status {
        grpc_params = format!(
            "http://{}:{}",
            env::var("GRPC_HOST").unwrap().as_str(),
            env::var("GRPC_PORT").unwrap().as_str()
        );
    }

    env_logger::Builder::from_env(
        Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, 
            format!("{},hyper=info,h2=info,rustls=info,tokio_postgres=info", log_level)),
    )
    .format_timestamp_millis()
    .init();

    // making panics look nicer
    panic::set_hook(Box::new(move |panic_info| {
        panic_info
            .payload()
            .downcast_ref::<embeds::WebsocketThreadError>().map_or_else(|| {
                error!(target: thread::current().name().unwrap(), "{}", panic_info);
            }, |e| {
                error!(target: thread::current().name().unwrap(), "Panicked on a custom error: {:?}", e);
            });
    }));

    let (ctrlc_logger_tx, ctrlc_logger_rx): (Sender<()>, Receiver<()>) =
        crossbeam_channel::unbounded();
    let (ctrlc_phrases_tx, ctrlc_phrases_rx): (Sender<()>, Receiver<()>) =
        crossbeam_channel::unbounded();
    let (ctrlc_embeds_tx, ctrlc_embeds_rx): (Sender<()>, Receiver<()>) =
        crossbeam_channel::unbounded();
    let (ctrlc_main_tx, ctrlc_main_rx): (Sender<()>, Receiver<()>) =
        crossbeam_channel::unbounded();

    let mut sleep_timer = 0;

    ctrlc::set_handler(move || {
        info!("Ctrl-C received, shutting down...");
        if ctrlc_logger_tx.send(()).is_err() {
            panic!("Couldn't shutdown the logger");
        }
        if ctrlc_phrases_tx.send(()).is_err() {
            panic!("Couldn't shutdown the phrases collector");
        }
        if ctrlc_embeds_tx.send(()).is_err() {
            panic!("Couldn't shutdown the embeds collector");
        }
        if ctrlc_main_tx.send(()).is_err() {
            panic!("Couldn't shutdown the main thread");
        }
    })
    .expect("Error setting Ctrl-C handler");

    'inner: loop {
        if ctrlc_main_rx.try_recv().is_ok() {
            return;
        }

        match sleep_timer {
            0 => {}
            1 => info!(
                "One of the main threads panicked, restarting in {} second",
                sleep_timer
            ),
            _ => info!(
                "One of the main threads panicked, restarting in {} seconds",
                sleep_timer
            ),
        }
        thread::sleep(Duration::from_secs(sleep_timer));

        let params_inner = params.clone();
        let params_inner_clone = params.clone();

        let grpc_params_inner = grpc_params.clone();
        let grpc_params_inner_clone = grpc_params.clone();

        let ctrlc_logger_rx_inner = ctrlc_logger_rx.clone();
        let ctrlc_phrases_rx_inner = ctrlc_phrases_rx.clone();
        let ctrlc_embeds_rx_inner = ctrlc_embeds_rx.clone();

        let logger_main = tokio::task::spawn_blocking(move || logger::main_loop(params_inner, grpc_params_inner, ctrlc_logger_rx_inner));
        thread::sleep(Duration::from_secs(4));
        let phrases_main = tokio::task::spawn_blocking(move || phrases::main_loop(params_inner_clone, grpc_params_inner_clone, ctrlc_phrases_rx_inner));
        thread::sleep(Duration::from_secs(4));
        let embeds_main = tokio::task::spawn_blocking(move || {
            embeds::main_loop(
                std::env::var("TWITCH_CLIENT_ID").ok(),
                std::env::var("TWITCH_CLIENT_SECRET").ok(),
                ctrlc_embeds_rx_inner,
            );
        });

        if ctrlc_main_rx.try_recv().is_ok() {
            logger_main.abort();
            break 'inner;
        }
    
        if logger_main.await.is_err() {
            match sleep_timer {
                0 => sleep_timer = 1,
                1..=32 => sleep_timer *= 2,
                _ => {}
            }
            continue 'inner;
        }

        if phrases_main.await.is_err() {
            match sleep_timer {
                0 => sleep_timer = 1,
                1..=32 => sleep_timer *= 2,
                _ => {}
            }
            continue 'inner;
        }

        if embeds_main.await.is_err() {
            match sleep_timer {
                0 => sleep_timer = 1,
                1..=32 => sleep_timer *= 2,
                _ => {}
            }
            continue 'inner;
        }
    }
}
