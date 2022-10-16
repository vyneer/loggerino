#![allow(clippy::cast_lossless)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]

use serde::{Deserialize, Serialize};
use std::{error::Error, fmt::Display, str::FromStr, string::ToString};
use time::{
    error::ComponentRange,
    macros::{date, time},
    Duration, OffsetDateTime, PrimitiveDateTime, UtcOffset,
};

static INITIAL_TIMESTAMP: PrimitiveDateTime =
    PrimitiveDateTime::new(date!(0001 - 01 - 01), time!(0:00));

const NUKE_COMMANDS: [&str; 1] = ["!nuke"];
const MEGANUKE_COMMANDS: [&str; 1] = ["!meganuke"];
const AEGIS_COMMANDS: [&str; 1] = ["!aegis"];
const AEGISSINGLE_COMMANDS: [&str; 4] = ["!aegissingle", "!an", "!unnuke", "!as"];
const MUTELINKS_COMMANDS: [&str; 4] = ["!mutelinks", "!mutelink", "!linkmute", "!linksmute"];
const BREAKINGNEWS_COMMANDS: [&str; 3] = ["!breakingnews", "!breaking", "!bn"];

pub mod grpc {
    #![allow(clippy::all)]

    tonic::include_proto!("grpc_timestamps");
}

pub enum TimeoutMsg {
    Ok,
    Shutdown,
}

#[derive(Deserialize, Debug)]
pub struct Message {
    pub data: String,
    pub nick: String,
    pub features: Vec<String>,
    pub timestamp: i64,
}

#[derive(Deserialize, Debug)]
pub struct JoinQuit {
    pub nick: String,
    pub features: Vec<String>,
    pub timestamp: i64,
}

#[derive(Deserialize, Debug)]
pub struct Names {
    pub connectioncount: i64,
    pub users: Vec<User>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct User {
    pub nick: String,
    pub features: Vec<String>,
}

#[derive(Deserialize, Debug)]
pub struct DGGPing {
    pub timestamp: i64,
}

#[derive(Deserialize, Debug)]
pub struct DGGPong {
    pub timestamp: i64,
}

#[derive(Eq, PartialEq)]
pub enum CommandType {
    Nuke,
    Meganuke,
    Aegis,
    AegisSingle,
    Mutelinks,
    BreakingNews,
}

impl FromStr for CommandType {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if NUKE_COMMANDS.contains(&input) {
            Ok(Self::Nuke)
        } else if MEGANUKE_COMMANDS.contains(&input) {
            Ok(Self::Meganuke)
        } else if AEGIS_COMMANDS.contains(&input) {
            Ok(Self::Aegis)
        } else if AEGISSINGLE_COMMANDS.contains(&input) {
            Ok(Self::AegisSingle)
        } else if MUTELINKS_COMMANDS.contains(&input) {
            Ok(Self::Mutelinks)
        } else if BREAKINGNEWS_COMMANDS.contains(&input) {
            Ok(Self::BreakingNews)
        } else {
            Err("Unknown command type".to_string())
        }
    }
}

impl ToString for CommandType {
    fn to_string(&self) -> String {
        match self {
            Self::Nuke => "nuke".to_string(),
            Self::Meganuke => "meganuke".to_string(),
            Self::Aegis => "aegis".to_string(),
            Self::AegisSingle => "aegissingle".to_string(),
            Self::Mutelinks => "mutelinks".to_string(),
            Self::BreakingNews => "breakingnews".to_string(),
        }
    }
}

#[derive(Debug)]
pub enum MessageType {
    Names,
    Msg,
    Ban,
    Unban,
    Mute,
    Unmute,
    Broadcast,
    Join,
    Quit,
    Subonly,
    Ping,
    Pong,
    Refresh,
}

impl FromStr for MessageType {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "NAMES" => Ok(Self::Names),
            "MSG" => Ok(Self::Msg),
            "BAN" => Ok(Self::Ban),
            "UNBAN" => Ok(Self::Unban),
            "MUTE" => Ok(Self::Mute),
            "UNMUTE" => Ok(Self::Unmute),
            "BROADCAST" => Ok(Self::Broadcast),
            "JOIN" => Ok(Self::Join),
            "QUIT" => Ok(Self::Quit),
            "SUBONLY" => Ok(Self::Subonly),
            "PING" => Ok(Self::Ping),
            "PONG" => Ok(Self::Pong),
            "REFRESH" => Ok(Self::Refresh),
            _ => Err("Unknown message type".to_string()),
        }
    }
}

// https://stackoverflow.com/questions/65976432/how-to-remove-first-and-last-character-of-a-string-in-rust
pub fn rem_first_and_last(value: &str) -> &str {
    let mut chars = value.chars();
    chars.next();
    chars.next_back();
    chars.as_str()
}

pub fn split_once(in_string: &str) -> (&str, &str) {
    let (first, second) = match in_string.split_once(' ') {
        Some(r) => r,
        None => (in_string, ""),
    };

    (first, second)
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum TimeConversionError {
    NoData,
    WrongVersion,
    InvalidLength,
    UnexpectedOffset,
    OffsetError(ComponentRange),
}

impl From<ComponentRange> for TimeConversionError {
    fn from(e: ComponentRange) -> Self {
        Self::OffsetError(e)
    }
}

impl Display for TimeConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoData => write!(f, "no data"),
            Self::WrongVersion => write!(f, "unsupported version"),
            Self::InvalidLength => write!(f, "invalid length"),
            Self::UnexpectedOffset => write!(f, "unexpected offset"),
            Self::OffsetError(e) => write!(f, "{}", e),
        }
    }
}

impl Error for TimeConversionError {}

#[allow(dead_code)]
pub fn convert_time_to_vec(time: OffsetDateTime) -> Result<Vec<u8>, TimeConversionError> {
    let mut version: u8 = 1;
    let offset_min: i16;
    let mut offset_sec: i8 = 0;

    if time.offset().is_utc() {
        offset_min = -1;
    } else {
        let mut offset = time.offset().whole_seconds();
        let buf_sec_offset = (offset % 60) as i8;
        if buf_sec_offset != 0 {
            version = 2;
            offset_sec = buf_sec_offset;
        }

        offset /= 60;
        if offset < -32768 || offset == -1 || offset > 32767 {
            return Err(TimeConversionError::UnexpectedOffset);
        }
        offset_min = offset as i16;
    }

    let sec = (time - INITIAL_TIMESTAMP.assume_utc()).whole_seconds();
    let nsec = i64::from(time.nanosecond());
    let mut enc: Vec<u8> = vec![
        version,
        (sec >> 56) as u8,
        (sec >> 48) as u8,
        (sec >> 40) as u8,
        (sec >> 32) as u8,
        (sec >> 24) as u8,
        (sec >> 16) as u8,
        (sec >> 8) as u8,
        sec as u8,
        (nsec >> 24) as u8,
        (nsec >> 16) as u8,
        (nsec >> 8) as u8,
        nsec as u8,
        (offset_min >> 8) as u8,
        offset_min as u8,
    ];
    if version == 2 {
        enc.push(offset_sec as u8);
    }

    Ok(enc)
}

#[allow(dead_code)]
pub fn convert_vec_to_time(time_vec: Vec<u8>) -> Result<OffsetDateTime, TimeConversionError> {
    if time_vec.is_empty() {
        return Err(TimeConversionError::NoData);
    }

    let version = time_vec[0];
    let mut desired_length = 1 + 8 + 4 + 2;

    match version {
        1 => (),
        2 => desired_length += 1,
        _ => return Err(TimeConversionError::WrongVersion),
    }

    if time_vec.len() != desired_length {
        return Err(TimeConversionError::InvalidLength);
    }

    let sec = (i64::from(time_vec[8]))
        | (i64::from(time_vec[7])) << 8
        | (i64::from(time_vec[6])) << 16
        | (i64::from(time_vec[5])) << 24
        | (i64::from(time_vec[4])) << 32
        | (i64::from(time_vec[3])) << 40
        | (i64::from(time_vec[2])) << 48
        | (i64::from(time_vec[1])) << 56;
    let nsec = (i32::from(time_vec[12]))
        | (i32::from(time_vec[11])) << 8
        | (i32::from(time_vec[10])) << 16
        | (i32::from(time_vec[9])) << 24;
    let mut offset = i32::from(i16::from(time_vec[14]) | (i16::from(time_vec[13])) << 8) * 60;
    if version == 2 {
        offset += i32::from(time_vec[15] as i8);
    }

    if offset == -60 {
        let beginning_offset = INITIAL_TIMESTAMP.assume_utc();
        Ok(beginning_offset + Duration::seconds(sec) + Duration::nanoseconds(i64::from(nsec)))
    } else {
        let utcoffset = match UtcOffset::from_whole_seconds(offset) {
            Ok(offset) => offset,
            Err(e) => return Err(e.into()),
        };
        let beginning_offset = INITIAL_TIMESTAMP.assume_offset(utcoffset);
        Ok(beginning_offset
            + Duration::seconds(sec + (offset as i64))
            + Duration::nanoseconds(i64::from(nsec)))
    }
}
