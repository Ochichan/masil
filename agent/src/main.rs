//! Read-only client for the core observation protocol. No daemon autostart.
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const MAX_FRAME: usize = 65_536;

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, x: bool) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_i64<E: de::Error>(self, x: i64) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_u64<E: de::Error>(self, x: u64) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_f64<E: de::Error>(self, x: f64) -> Result<Strict, E> {
                serde_json::Number::from_f64(x)
                    .map(|n| Strict(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, x: &str) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_none<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut values = Vec::new();
                while let Some(Strict(value)) = a.next_element()? {
                    if values.len() == 64 {
                        return Err(de::Error::custom("array limit"));
                    }
                    values.push(value);
                }
                Ok(Strict(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut values = Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    if values.len() == 32 {
                        return Err(de::Error::custom("object limit"));
                    }
                    let Strict(value) = a.next_value()?;
                    values.insert(key, value);
                }
                Ok(Strict(Value::Object(values)))
            }
        }
        d.deserialize_any(V)
    }
}

fn bounded(value: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match value {
        Value::Array(a) => a.iter().all(|v| bounded(v, depth + 1)),
        Value::Object(m) => m.values().all(|v| bounded(v, depth + 1)),
        _ => true,
    }
}

fn receive(stream: &mut UnixStream, request_id: &str) -> Result<Value, String> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).map_err(|e| e.to_string())?;
    let n = u32::from_be_bytes(header) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err("response exceeds frame limit".into());
    }
    let mut body = vec![0; n];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    let Strict(value) = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    if !bounded(&value, 1) || value["v"] != 1 || value["request_id"] != request_id {
        return Err("invalid response envelope".into());
    }
    Ok(value)
}

fn exchange(stream: &mut UnixStream, value: Value) -> Result<Value, String> {
    let body = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
    if body.len() > 8192 {
        return Err("request exceeds core frame limit".into());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(&body).map_err(|e| e.to_string())?;
    receive(
        stream,
        value["request_id"].as_str().ok_or("missing request ID")?,
    )
}

fn execute() -> Result<i32, String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] {
        println!(
            "rmux-agent 0.1.0 — core observation client\n\nUsage: rmux-agent --socket PATH hello|inventory|snapshot %N|stats\n\nExplicit core bridge required: RMUX_BRIDGE_SOCKET=/private/path/observe.sock rmux ...\nNo provider daemon, prompt submission, approval or autostart is implemented."
        );
        return Ok(0);
    }
    if args == ["--version"] {
        println!("rmux-agent 0.1.0");
        return Ok(0);
    }
    if args.len() < 3 || args[0] != "--socket" {
        return Err("expected --socket PATH command".into());
    }
    let command = args[2].as_str();
    if !matches!(command, "hello" | "inventory" | "snapshot" | "stats") {
        return Err("unsupported command; see --help".into());
    }
    let expected_len = if command == "snapshot" { 4 } else { 3 };
    if args.len() != expected_len {
        return Err("incorrect arguments; see --help".into());
    }
    let mut stream =
        UnixStream::connect(&args[1]).map_err(|e| format!("bridge unavailable: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    let hello = exchange(
        &mut stream,
        json!({"v":1,"kind":"hello","request_id":"hello"}),
    )?;
    if hello["kind"] == "error" || command == "hello" {
        println!(
            "{}",
            serde_json::to_string_pretty(&hello).map_err(|e| e.to_string())?
        );
        return Ok(if hello["kind"] == "error" { 5 } else { 0 });
    }
    let mut request = json!({"v":1,"kind":command,"request_id":"query"});
    if command == "snapshot" {
        request["pane_id"] = args[3].clone().into();
    }
    // Stream inventory pages individually, preserving the core's revision fence.
    loop {
        let response = exchange(&mut stream, request.clone())?;
        println!(
            "{}",
            serde_json::to_string(&response).map_err(|e| e.to_string())?
        );
        if response["kind"] == "error" {
            return Ok(5);
        }
        if command != "inventory" || response["next_cursor"].is_null() {
            break;
        }
        request["cursor"] = response["next_cursor"].clone();
        request["revision"] = response["revision"].clone();
    }
    Ok(0)
}

fn main() {
    match execute() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("rmux-agent: {error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_nested_duplicate_fields() {
        assert!(serde_json::from_str::<Strict>(r#"{"a":{"x":1,"x":2}}"#).is_err());
    }
    #[test]
    fn accepts_unicode_and_enforces_depth() {
        let Strict(value) = serde_json::from_str(r#"{"text":"한글\n中文","null":null}"#).unwrap();
        assert!(bounded(&value, 1));
        let mut deep = Value::Null;
        for _ in 0..9 {
            deep = json!({"nested":deep});
        }
        assert!(!bounded(&deep, 1));
    }
}
