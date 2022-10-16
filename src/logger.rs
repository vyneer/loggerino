#![warn(clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::match_wild_err_arm,
    clippy::type_complexity,
    clippy::too_many_arguments
)]

use crossbeam_channel::{Receiver, Sender};
use fancy_regex::Regex;
use futures_util::{future, pin_mut, StreamExt};
use lazy_static::lazy_static;
use log::{debug, error, info, trace};
use rusqlite::{params, Connection};
use rust_decimal::prelude::{Decimal, ToPrimitive};
use std::{
    fs,
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
    {panic, thread},
};
use time::OffsetDateTime;
use tokio::time::timeout;
use tokio_postgres::{connect, NoTls};
use tokio_tungstenite::{connect_async, tungstenite::Message::Pong};
use tonic::transport::Channel;
use url::Url;

use crate::util::{
    grpc::{status_client::StatusClient, Aegis, Mutelinks, Nuke},
    split_once, CommandType, JoinQuit, Message, MessageType, Names, TimeoutMsg,
};

lazy_static! {
    static ref NUKE_REGEX: Regex = Regex::new(r"(\d+[HMDSWwhmds])?\s?(?:\/(.*)\/)?(.*)").unwrap();
    static ref MUTELINKS_REGEX: Regex =
        Regex::new(r"(?P<state>on|off|all)(?:(?:\s+)(?P<time>\d+[HMDSWwhmds]))?").unwrap();
}

#[tokio::main]
async fn websocket_thread_func(
    params: String,
    timer_tx: Sender<TimeoutMsg>,
    ctrlc_inner_rx: Receiver<()>,
    ctrlc_outer_tx: Sender<()>,
    grpc_params: String,
) {
    let mut io_error_counter = 0;

    let mut grpc_client: Option<StatusClient<Channel>>;
    if grpc_params.is_empty() {
        grpc_client = None;
    } else {
        grpc_client = Some(StatusClient::connect(grpc_params).await.unwrap());
    }

    'ioerrortracker: loop {
        if io_error_counter > 3 {
            let io_error_sleep = io_error_counter - 3;
            debug!(
                "Too many IO errors in a row ({}), sleeping before connecting",
                io_error_counter
            );
            thread::sleep(Duration::from_secs(io_error_sleep));
        }
        let mut sqlite_conn = Connection::open("./data/featdb.db").unwrap();

        // since we can't move a pg connection from one thread to another easily
        // we just recreate it
        let (mut pg_conn, pg_conn2) = connect(params.as_str(), NoTls).await.unwrap();

        let timer_tx_pg_error = timer_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = pg_conn2.await {
                error!("Postgres connection error: {}", e);
                timer_tx_pg_error.send(TimeoutMsg::Shutdown).unwrap();
            }
        });

        let ws = connect_async(Url::parse("wss://chat.destiny.gg/ws").unwrap());

        let (socket, response) = match timeout(Duration::from_secs(10), ws).await {
            Ok(ws) => {
                let (socket, response) = match ws {
                    Ok((socket, response)) => {
                        assert!(
                            response.status() == 101,
                            "Response isn't 101, can't continue (restarting the thread)."
                        );
                        (socket, response)
                    }
                    Err(e) => {
                        panic!("Unexpected error, restarting the thread: {}", e)
                    }
                };
                (socket, response)
            }
            Err(e) => {
                panic!("Connection timed out, restarting the thread: {}", e);
            }
        };

        info!("Connected to the server");
        debug!("Response HTTP code: {}", response.status());

        let (stdin_tx, stdin_rx) = futures_channel::mpsc::unbounded();
        let (write, mut read) = socket.split();

        let stdin_to_ws = stdin_rx.map(Ok).forward(write);
        let ws_to_stdout = {
            while let Some(msg) = read.next().await {
                let msg_og = match msg {
                    Ok(msg_og) => {
                        if io_error_counter != 0 {
                            io_error_counter = 0;
                        }
                        msg_og
                    }
                    Err(tokio_tungstenite::tungstenite::Error::Io(e)) => {
                        error!("Tungstenite IO error, restarting the loop: {}", e);
                        io_error_counter += 1;
                        continue 'ioerrortracker;
                    }
                    Err(e) => {
                        panic!(
                            "Some kind of other error occured, restarting the thread: {}",
                            e
                        );
                    }
                };
                // send () to our timer channel,
                // letting that other thread know we're alive
                timer_tx.send(TimeoutMsg::Ok).unwrap();
                // try and receive ctrl-c signal to shutdown
                if ctrlc_inner_rx.try_recv().is_ok() {
                    ctrlc_outer_tx.send(()).unwrap();
                    timer_tx.send(TimeoutMsg::Shutdown).unwrap();
                    return;
                }
                if msg_og.is_text() {
                    let (msg_type, msg_data) = split_once(msg_og.to_text().unwrap());
                    if let Ok(t) = MessageType::from_str(msg_type) {
                        match t {
                            MessageType::Names => {
                                let tr_sqlite = sqlite_conn.transaction().unwrap();
                                let tr_pg = pg_conn.transaction().await.unwrap();
                                let names_des: Names = serde_json::from_str(msg_data).unwrap();
                                for user in names_des.users {
                                    tr_sqlite.execute(
                                        "REPLACE INTO dggfeat (username, features) VALUES (?, ?)",
                                        params![
                                            user.nick,
                                            serde_json::to_string(&user.features).unwrap()
                                        ],
                                    )
                                    .unwrap();
                                    if user.features.contains(&"flair15".to_string()) {
                                        tr_pg.execute(
                                            "INSERT INTO chatters (username, birthday) VALUES ($1, TO_TIMESTAMP($2/1000.0)) ON CONFLICT (username) DO UPDATE SET birthday = COALESCE(chatters.birthday, EXCLUDED.birthday)", 
                                            &[&user.nick, &Decimal::new(OffsetDateTime::now_utc().unix_timestamp(), 0)],
                                        ).await.unwrap();
                                    }
                                    tr_pg.execute(
                                        "INSERT INTO chatters (username, features, firstseen, lastseen) VALUES ($1, $2, NOW(), NOW()) ON CONFLICT (username) DO UPDATE SET features = EXCLUDED.features, firstseen = COALESCE(chatters.firstseen, EXCLUDED.firstseen), lastseen = EXCLUDED.lastseen", 
                                        &[&user.nick, &format!("{{{}}}", user.features.join(","))],
                                    ).await.unwrap();
                                }
                                tr_sqlite.commit().unwrap();
                                tr_pg.commit().await.unwrap();
                                debug!("Updated the features of all connected users (got a NAMES message type)");
                            }
                            MessageType::Msg => {
                                let tr = pg_conn.transaction().await.unwrap();
                                let msg_des: Message = serde_json::from_str(msg_data).unwrap();
                                tr.execute(
                                        "INSERT INTO logs (time, username, features, message) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4)", 
                                        &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &format!("{{{}}}", msg_des.features.join(",")), &msg_des.data],
                                    ).await.unwrap();
                                trace!(
                                    "Added a log to the 'logs' table - | [{}] {{f: {}}} {}: {} |",
                                    msg_des.timestamp,
                                    msg_des.features.len(),
                                    msg_des.nick,
                                    msg_des.data
                                );
                                if msg_des.features.contains(&"flair15".to_string()) {
                                    tr.execute(
                                        "INSERT INTO chatters (username, birthday) VALUES ($1, TO_TIMESTAMP($2/1000.0)) ON CONFLICT (username) DO UPDATE SET birthday = COALESCE(chatters.birthday, EXCLUDED.birthday)", 
                                        &[&msg_des.nick, &Decimal::new(msg_des.timestamp, 0)],
                                    ).await.unwrap();
                                }
                                tr.execute(
                                    "INSERT INTO chatters (username, firstmessage, lastmessage, features, msgcount, lastseen) VALUES ($1, TO_TIMESTAMP($2/1000.0), TO_TIMESTAMP($2/1000.0), $3, $4, TO_TIMESTAMP($2/1000.0)) ON CONFLICT (username) DO UPDATE SET firstmessage = COALESCE(chatters.firstmessage, EXCLUDED.firstmessage), lastmessage = EXCLUDED.lastmessage, features = EXCLUDED.features, msgcount = COALESCE(chatters.msgcount, EXCLUDED.msgcount) + 1, lastseen = EXCLUDED.lastseen", 
                                    &[&msg_des.nick, &Decimal::new(msg_des.timestamp, 0), &format!("{{{}}}", msg_des.features.join(",")), &0i32],
                                ).await.unwrap();

                                let lc_data = msg_des.data.to_lowercase();
                                if msg_des.features.contains(&"admin".to_string())
                                    || msg_des.features.contains(&"moderator".to_string())
                                {
                                    let (command, params) = split_once(lc_data.as_str());
                                    if let Ok(t) = CommandType::from_str(command) {
                                        match t {
                                            CommandType::Nuke => {
                                                tr.execute(
                                                            "INSERT INTO nukes (time, username, features, message) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4)", 
                                                            &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &format!("{{{}}}", msg_des.features.join(",")), &msg_des.data],
                                                        ).await.unwrap();
                                                debug!("Added a nuke log to the 'nukes' table - | [{}] {{f: {}}} {}: {} |", msg_des.timestamp, msg_des.features.len(), msg_des.nick, msg_des.data);
                                                match grpc_client.as_mut() {
                                                    Some(grpc_client) => {
                                                        if let Ok(Some(m)) =
                                                            NUKE_REGEX.captures(params)
                                                        {
                                                            let duration = m
                                                                .get(1)
                                                                .map_or("10m".to_string(), |dur| {
                                                                    dur.as_str().to_string()
                                                                });
                                                            let word = m
                                                                .get(3)
                                                                .map_or("".to_string(), |w| {
                                                                    w.as_str().to_string()
                                                                });
                                                            let regex = m.get(2);
                                                            let word_to_mute = match regex {
                                                                Some(r) => format!("/{}/", r.as_str().to_string()),
                                                                None => word,
                                                            };
                                                            let stamp_secs =
                                                                msg_des.timestamp / 1000;
                                                            let stamp_micros =
                                                                ((msg_des.timestamp % 1000)
                                                                    * 1_000_000)
                                                                    .to_i32()
                                                                    .unwrap();
                                                            let stamp = prost_types::Timestamp {
                                                                seconds: stamp_secs,
                                                                nanos: stamp_micros,
                                                            };
                                                            let nuke_request =
                                                                tonic::Request::new(Nuke {
                                                                    time: Some(stamp.clone()),
                                                                    r#type: t.to_string(),
                                                                    duration: duration.clone(),
                                                                    word: word_to_mute.clone(),
                                                                });
                                                            debug!("Sending a gRPC nuke event to the API server - | [{}] {} - {} - {} |", stamp, t.to_string(), word_to_mute, duration);
                                                            grpc_client
                                                                .receive_nuke(nuke_request)
                                                                .await
                                                                .unwrap();
                                                        }
                                                    }
                                                    None => (),
                                                }
                                            }
                                            CommandType::Meganuke => {
                                                tr.execute(
                                                            "INSERT INTO nukes (time, username, features, message) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4)", 
                                                            &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &format!("{{{}}}", msg_des.features.join(",")), &msg_des.data],
                                                        ).await.unwrap();
                                                debug!("Added a meganuke log to the 'nukes' table - | [{}] {{f: {}}} {}: {} |", msg_des.timestamp, msg_des.features.len(), msg_des.nick, msg_des.data);
                                                match grpc_client.as_mut() {
                                                    Some(grpc_client) => {
                                                        if let Ok(Some(m)) =
                                                            NUKE_REGEX.captures(params)
                                                        {
                                                            let duration = m
                                                                .get(1)
                                                                .map_or("10m".to_string(), |dur| {
                                                                    dur.as_str().to_string()
                                                                });
                                                            let word = m
                                                                .get(3)
                                                                .map_or("".to_string(), |w| {
                                                                    w.as_str().to_string()
                                                                });
                                                            let regex = m.get(2);
                                                            let word_to_mute = match regex {
                                                                Some(r) => format!("/{}/", r.as_str().to_string()),
                                                                None => word,
                                                            };
                                                            let stamp_secs =
                                                                msg_des.timestamp / 1000;
                                                            let stamp_micros =
                                                                ((msg_des.timestamp % 1000)
                                                                    * 1_000_000)
                                                                    .to_i32()
                                                                    .unwrap();
                                                            let stamp = prost_types::Timestamp {
                                                                seconds: stamp_secs,
                                                                nanos: stamp_micros,
                                                            };
                                                            let nuke_request =
                                                                tonic::Request::new(Nuke {
                                                                    time: Some(stamp.clone()),
                                                                    r#type: t.to_string(),
                                                                    duration: duration.clone(),
                                                                    word: word_to_mute.clone(),
                                                                });
                                                            debug!("Sending a gRPC meganuke event to the API server - | [{}] {} - {} - {} |", stamp, t.to_string(), word_to_mute, duration);
                                                            grpc_client
                                                                .receive_nuke(nuke_request)
                                                                .await
                                                                .unwrap();
                                                        }
                                                    }
                                                    None => (),
                                                }
                                            }
                                            CommandType::Aegis | CommandType::AegisSingle => {
                                                tr.execute(
                                                    "INSERT INTO nukes (time, username, features, message) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4)", 
                                                    &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &format!("{{{}}}", msg_des.features.join(",")), &msg_des.data],
                                                ).await.unwrap();
                                                debug!("Added an aegis log to the 'nukes' table - | [{}] {{f: {}}} {}: {} |", msg_des.timestamp, msg_des.features.len(), msg_des.nick, msg_des.data);
                                                match grpc_client.as_mut() {
                                                    Some(grpc_client) => {
                                                        let stamp_secs = msg_des.timestamp / 1000;
                                                        let stamp_micros = ((msg_des.timestamp
                                                            % 1000)
                                                            * 1_000_000)
                                                            .to_i32()
                                                            .unwrap();
                                                        let stamp = prost_types::Timestamp {
                                                            seconds: stamp_secs,
                                                            nanos: stamp_micros,
                                                        };
                                                        let grpc_type = if t == CommandType::Aegis {
                                                            0
                                                        } else {
                                                            1
                                                        };
                                                        let aegis_request =
                                                            tonic::Request::new(Aegis {
                                                                time: Some(stamp.clone()),
                                                                r#type: grpc_type,
                                                                word: params.to_string(),
                                                            });
                                                        if params.is_empty() {
                                                            debug!("Sending a gRPC aegis event to the API server - | [{}] {} |", stamp, t.to_string());
                                                        } else {
                                                            debug!("Sending a gRPC aegissingle event to the API server - | [{}] {} - {} |", stamp, t.to_string(), params);
                                                        }
                                                        grpc_client
                                                            .receive_aegis(aegis_request)
                                                            .await
                                                            .unwrap();
                                                    }
                                                    None => (),
                                                }
                                            }
                                            CommandType::Mutelinks => {
                                                tr.execute(
                                                            "INSERT INTO mutelinks (time, username, features, message) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4)", 
                                                            &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &format!("{{{}}}", msg_des.features.join(",")), &msg_des.data],
                                                        ).await.unwrap();
                                                debug!("Added a mutelinks log to the 'mutelinks' table - | [{}] {{f: {}}} {}: {} |", msg_des.timestamp, msg_des.features.len(), msg_des.nick, msg_des.data);
                                                match grpc_client.as_mut() {
                                                    Some(grpc_client) => {
                                                        if let Ok(Some(m)) =
                                                            MUTELINKS_REGEX.captures(params)
                                                        {
                                                            let duration = m
                                                                .name("time")
                                                                .map_or("10m".to_string(), |dur| {
                                                                    dur.as_str().to_string()
                                                                });
                                                            let status = m
                                                                .name("state")
                                                                .unwrap()
                                                                .as_str()
                                                                .to_string();
                                                            let stamp_secs =
                                                                msg_des.timestamp / 1000;
                                                            let stamp_micros =
                                                                ((msg_des.timestamp % 1000)
                                                                    * 1_000_000)
                                                                    .to_i32()
                                                                    .unwrap();
                                                            let stamp = prost_types::Timestamp {
                                                                seconds: stamp_secs,
                                                                nanos: stamp_micros,
                                                            };
                                                            let mutelinks_request =
                                                                tonic::Request::new(Mutelinks {
                                                                    time: Some(stamp.clone()),
                                                                    status: status.clone(),
                                                                    duration: duration.clone(),
                                                                    user: msg_des.nick.clone(),
                                                                });
                                                            debug!("Sending a gRPC mutelinks event to the API server - | [{}] {}: {} - {} |", stamp, msg_des.nick.clone(), status, duration);
                                                            grpc_client
                                                                .receive_mutelinks(
                                                                    mutelinks_request,
                                                                )
                                                                .await
                                                                .unwrap();
                                                        }
                                                    }
                                                    None => (),
                                                }
                                            }
                                            CommandType::BreakingNews => {
                                                tr.execute(
                                                            "INSERT INTO breakingnews (time, username, features, message) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4)", 
                                                            &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &format!("{{{}}}", msg_des.features.join(",")), &msg_des.data],
                                                        ).await.unwrap();
                                                debug!("Added a breakingnews log to the 'breakingnews' table - | [{}] {{f: {}}} {}: {} |", msg_des.timestamp, msg_des.features.len(), msg_des.nick, msg_des.data);
                                            }
                                        }
                                    };
                                }
                                if msg_des.features.contains(&"bot".to_string())
                                    && msg_des.data.starts_with("Dropping the NUKE on")
                                {
                                    tr.execute(
                                            "INSERT INTO nukes (time, username, features, message) VALUES (TO_TIMESTAMP($1/1000.0), $2, $3, $4)", 
                                            &[&Decimal::new(msg_des.timestamp, 0), &msg_des.nick, &format!("{{{}}}", msg_des.features.join(",")), &msg_des.data],
                                        ).await.unwrap();
                                    debug!("Added the bot's nuke victim count log to the 'nukes' table - | [{}] {{f: {}}} {}: {} |", msg_des.timestamp, msg_des.features.len(), msg_des.nick, msg_des.data);
                                }
                                tr.commit().await.unwrap();
                                sqlite_conn
                                    .execute(
                                        "REPLACE INTO dggfeat (username, features) VALUES (?, ?)",
                                        params![
                                            msg_des.nick,
                                            serde_json::to_string(&msg_des.features).unwrap()
                                        ],
                                    )
                                    .unwrap();
                            }
                            MessageType::Join => {
                                let joinquit_des: JoinQuit =
                                    serde_json::from_str(msg_data).unwrap();
                                sqlite_conn
                                    .execute(
                                        "REPLACE INTO dggfeat (username, features) VALUES (?, ?)",
                                        params![
                                            joinquit_des.nick,
                                            serde_json::to_string(&joinquit_des.features).unwrap()
                                        ],
                                    )
                                    .unwrap();
                                pg_conn.execute(
                                    "INSERT INTO chatters (username, firstseen, features, msgcount, lastseen) VALUES ($1, TO_TIMESTAMP($2/1000.0), $3, $4, TO_TIMESTAMP($2/1000.0)) ON CONFLICT (username) DO UPDATE SET features = EXCLUDED.features, lastseen = COALESCE(chatters.lastseen, EXCLUDED.lastseen)", 
                                    &[&joinquit_des.nick, &Decimal::new(joinquit_des.timestamp, 0),&format!("{{{}}}", joinquit_des.features.join(",")), &0i32],
                                ).await.unwrap();
                                if joinquit_des.features.contains(&"flair15".to_string()) {
                                    pg_conn.execute(
                                        "INSERT INTO chatters (username, birthday) VALUES ($1, TO_TIMESTAMP($2/1000.0)) ON CONFLICT (username) DO UPDATE SET birthday = COALESCE(chatters.birthday, EXCLUDED.birthday)", 
                                        &[&joinquit_des.nick, &Decimal::new(joinquit_des.timestamp, 0)],
                                    ).await.unwrap();
                                }
                            }
                            MessageType::Quit => {
                                let joinquit_des: JoinQuit =
                                    serde_json::from_str(msg_data).unwrap();
                                sqlite_conn
                                    .execute(
                                        "REPLACE INTO dggfeat (username, features) VALUES (?, ?)",
                                        params![
                                            joinquit_des.nick,
                                            serde_json::to_string(&joinquit_des.features).unwrap()
                                        ],
                                    )
                                    .unwrap();
                                pg_conn.execute(
                                    "INSERT INTO chatters (username, lastseen, features, msgcount, firstseen) VALUES ($1, TO_TIMESTAMP($2/1000.0), $3, $4, TO_TIMESTAMP($2/1000.0)) ON CONFLICT (username) DO UPDATE SET features = EXCLUDED.features, lastseen = EXCLUDED.lastseen, firstseen = COALESCE(chatters.firstseen, EXCLUDED.firstseen)", 
                                    &[&joinquit_des.nick, &Decimal::new(joinquit_des.timestamp, 0),&format!("{{{}}}", joinquit_des.features.join(",")), &0i32],
                                ).await.unwrap();
                                if joinquit_des.features.contains(&"flair15".to_string()) {
                                    pg_conn.execute(
                                        "INSERT INTO chatters (username, birthday) VALUES ($1, TO_TIMESTAMP($2/1000.0)) ON CONFLICT (username) DO UPDATE SET birthday = COALESCE(chatters.birthday, EXCLUDED.birthday)", 
                                        &[&joinquit_des.nick, &Decimal::new(joinquit_des.timestamp, 0)],
                                    ).await.unwrap();
                                }
                            }
                            _ => (),
                        }
                    }
                }
                if msg_og.is_ping() {
                    let ping_og = msg_og.clone().into_data();
                    stdin_tx.unbounded_send(Pong(ping_og)).unwrap();
                }
                if msg_og.is_close() {
                    error!("Server closed the connection, restarting the loop");
                    continue 'ioerrortracker;
                }
            }
            read.into_future()
        };

        pin_mut!(stdin_to_ws, ws_to_stdout);
        future::select(stdin_to_ws, ws_to_stdout).await;
    }
}

#[tokio::main]
pub async fn main_loop(params: String, grpc_params: String, ctrlc_inner: Receiver<()>) {
    if ctrlc_inner.try_recv().is_ok() {
        return;
    }

    let path = "./data";
    match fs::create_dir_all(path) {
        Ok(_) => (),
        Err(_) => panic!("Couldn't create a 'data' folder, not sure what went wrong, panicking."),
    }

    let sqlite_conn = Connection::open("./data/featdb.db").unwrap();

    sqlite_conn
        .execute(
            "CREATE TABLE IF NOT EXISTS dggfeat (username text, features text)",
            [],
        )
        .unwrap();
    sqlite_conn
        .execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS usernames ON dggfeat(username)",
            [],
        )
        .unwrap();

    sqlite_conn.close().unwrap();

    let (pg_conn, pg_conn2) = connect(params.as_str(), NoTls).await.unwrap();

    let handle = tokio::spawn(async move {
        if let Err(e) = pg_conn2.await {
            error!("Postgres connection error: {}", e);
        }
    });

    let check = pg_conn
        .query_one(
            "select exists(select * from information_schema.tables where table_name=$1)",
            &[&"logs"],
        )
        .await
        .unwrap();
    let bool_check: bool = check.get(0);
    if !bool_check {
        pg_conn.batch_execute(
			"
			CREATE TABLE IF NOT EXISTS logs (time TIMESTAMPTZ NOT NULL, username TEXT NOT NULL, features TEXT NOT NULL, message TEXT NOT NULL);
			SELECT create_hypertable('logs', 'time');
			CREATE TABLE IF NOT EXISTS nukes (time TIMESTAMPTZ NOT NULL, username TEXT NOT NULL, features TEXT NOT NULL, message TEXT NOT NULL);
			SELECT create_hypertable('nukes', 'time');
			CREATE TABLE IF NOT EXISTS mutelinks (time TIMESTAMPTZ NOT NULL, username TEXT NOT NULL, features TEXT NOT NULL, message TEXT NOT NULL);
			SELECT create_hypertable('mutelinks', 'time');
			CREATE TABLE IF NOT EXISTS breakingnews (time TIMESTAMPTZ NOT NULL, username TEXT NOT NULL, features TEXT NOT NULL, message TEXT NOT NULL);
			SELECT create_hypertable('breakingnews', 'time');
        ").await
		.unwrap();
    }

    pg_conn.batch_execute(
        "CREATE TABLE IF NOT EXISTS chatters (id SERIAL PRIMARY KEY, username TEXT NOT NULL UNIQUE, birthday TIMESTAMPTZ, firstmessage TIMESTAMPTZ, lastmessage TIMESTAMPTZ, firstseen TIMESTAMPTZ, lastseen TIMESTAMPTZ, features TEXT, msgcount INTEGER);").await
    .unwrap();

    handle.abort();

    let sleep_timer = Arc::new(AtomicU64::new(0));
    let (ctrlc_outer_tx, ctrlc_outer_rx): (Sender<()>, Receiver<()>) =
        crossbeam_channel::unbounded();

    'outer: loop {
        let cloned_ctrlc_outer_tx_ws = ctrlc_outer_tx.clone();
        let cloned_ctrlc_inner_rx = ctrlc_inner.clone();

        if cloned_ctrlc_inner_rx.try_recv().is_ok() {
            return;
        }

        if ctrlc_outer_rx.try_recv().is_ok() {
            return;
        }

        let sleep_timer_inner = Arc::clone(&sleep_timer);
        let params = params.clone();
        let grpc_params = grpc_params.clone();

        // timeout channels
        let (timer_tx, timer_rx): (Sender<TimeoutMsg>, Receiver<TimeoutMsg>) =
            crossbeam_channel::unbounded();

        match sleep_timer.load(Ordering::Acquire) {
            0 => {}
            1 => info!(
                "One of the threads panicked, restarting in {} second",
                sleep_timer.load(Ordering::Acquire)
            ),
            _ => info!(
                "One of the threads panicked, restarting in {} seconds",
                sleep_timer.load(Ordering::Acquire)
            ),
        }
        thread::sleep(Duration::from_secs(sleep_timer.load(Ordering::Acquire)));

        // this thread checks for the timeouts in the websocket thread
        // if there's nothing in the ws for a minute, panic
        let timeout_thread = thread::Builder::new()
            .name("loggerino::logger::timeout_thread".to_string())
            .spawn(move || loop {
                match timer_rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(m) => match m {
                        TimeoutMsg::Ok => {
                            if sleep_timer_inner.load(Ordering::Acquire) != 0 {
                                sleep_timer_inner.store(0, Ordering::Release);
                            }
                        }
                        TimeoutMsg::Shutdown => {
                            break;
                        }
                    },
                    Err(e) => {
                        panic!("Lost connection, terminating the timeout thread: {}", e);
                    }
                }
            })
            .unwrap();

        let ctrlc_inner_rx = ctrlc_inner.clone();

        // the main websocket thread that does all the hard work
        let ws_thread = thread::Builder::new()
            .name("loggerino::logger::websocket_thread".to_string())
            .spawn(move || {
                websocket_thread_func(
                    params,
                    timer_tx,
                    ctrlc_inner_rx,
                    cloned_ctrlc_outer_tx_ws,
                    grpc_params,
                );
            })
            .unwrap();

        if timeout_thread.join().is_err() {
            match sleep_timer.load(Ordering::Acquire) {
                0 => sleep_timer.store(1, Ordering::Release),
                1..=32 => {
                    sleep_timer.store(sleep_timer.load(Ordering::Acquire) * 2, Ordering::Release)
                }
                _ => {}
            }
            continue 'outer;
        }
        if ws_thread.join().is_err() {
            match sleep_timer.load(Ordering::Acquire) {
                0 => sleep_timer.store(1, Ordering::Release),
                1..=32 => {
                    sleep_timer.store(sleep_timer.load(Ordering::Acquire) * 2, Ordering::Release)
                }
                _ => {}
            }
            continue 'outer;
        }
    }
}
