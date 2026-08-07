//! `oxide_kv_cli` — a minimal command-line Oxide-KV client.
//!
//! Demonstrates using the `oxide-kv-client` crate from a binary:
//!
//!     cargo run --example oxide_kv_cli -- set hello world
//!     cargo run --example oxide_kv_cli -- get hello
//!     cargo run --example oxide_kv_cli -- delete hello
//!
//! Override the endpoint via `--host 127.0.0.1 --port 9101`.

use oxide_kv_client::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: {} <set|get|delete> <key> [value] \
             [--host HOST] [--port PORT]",
            args.first().map(String::as_str).unwrap_or("oxide_kv_cli")
        );
        std::process::exit(2);
    }

    // Parse flags + positional args. Tiny hand-rolled parser — clap is
    // overkill for three subcommands.
    let mut host = "127.0.0.1".to_owned();
    let mut port: u16 = 9101;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                host = args[i + 1].clone();
                i += 2;
            }
            "--port" => {
                port = args[i + 1].parse()?;
                i += 2;
            }
            other => {
                positional.push(other.to_owned());
                i += 1;
            }
        }
    }

    let cmd = positional.first().map(String::as_str).unwrap_or("");
    let mut client = Client::connect(&host, port).await?;

    match cmd {
        "set" => {
            let key = positional
                .get(1)
                .ok_or("set requires <key> <value>")?
                .clone();
            let value = positional
                .get(2)
                .ok_or("set requires <key> <value>")?
                .clone();
            let idx = client.set(&key, &value).await?;
            println!("{idx}");
        }
        "get" => {
            let key = positional.get(1).ok_or("get requires <key>")?.clone();
            match client.get(&key).await? {
                Some(v) => println!("{v}"),
                None => {
                    println!("(nil)");
                    std::process::exit(1);
                }
            }
        }
        "delete" => {
            let key = positional.get(1).ok_or("delete requires <key>")?.clone();
            let idx = client.delete(&key).await?;
            println!("{idx}");
        }
        _ => {
            eprintln!("unknown subcommand: {cmd}");
            std::process::exit(2);
        }
    }

    Ok(())
}
