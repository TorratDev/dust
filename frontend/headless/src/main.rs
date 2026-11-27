//! `dust-headless`: serves the dust-agent gRPC API with no window.
//!
//! Prints `dust-headless listening on <addr>` on stdout once it accepts connections; with
//! `--port 0` that line is how callers learn the chosen port.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    process::ExitCode,
};

const USAGE: &str = "\
Usage: dust-headless [--port N] [--host ADDR]

  --port N     TCP port to listen on (default 0: pick a free port)
  --host ADDR  address to bind (default 127.0.0.1)";

fn parse_args() -> Result<SocketAddr, String> {
    let mut port = 0;
    let mut host = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let (name, inline_value) = match arg.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (arg, None),
        };
        let mut value = || {
            inline_value
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match name.as_str() {
            "--port" => {
                let value = value()?;
                port = value
                    .parse()
                    .map_err(|_| format!("invalid port: {value:?}"))?;
            }
            "--host" => {
                let value = value()?;
                host = value
                    .parse()
                    .map_err(|_| format!("invalid host address: {value:?}"))?;
            }
            "-h" | "--help" => return Err(String::new()),
            _ => return Err(format!("unknown argument: {name}")),
        }
    }
    Ok(SocketAddr::new(host, port))
}

#[tokio::main]
async fn main() -> ExitCode {
    let addr = match parse_args() {
        Ok(addr) => addr,
        Err(err) => {
            if err.is_empty() {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let (listener, local_addr) = match dust_agent_grpc::bind(addr).await {
        Ok(bound) => bound,
        Err(err) => {
            eprintln!("error: couldn't listen on {addr}: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("dust-headless listening on {local_addr}");

    if let Err(err) = dust_agent_grpc::serve(listener).await {
        eprintln!("error: server failed: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
