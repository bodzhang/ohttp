#![deny(clippy::pedantic)]

use std::{io::Cursor, net::SocketAddr, path::PathBuf, sync::Arc};

use futures::StreamExt;
use futures_util::stream::{once, unfold};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Response, Url,
};
use tokio::sync::Mutex;

use bhttp::{Message, Mode, StatusCode};
use ohttp::{
    hpke::{Aead, Kdf, Kem},
    KeyConfig, Server as OhttpServer, ServerResponse, SymmetricSuite,
};
use structopt::StructOpt;
use warp::hyper::Body;
use warp::Filter;

use cgpuvm_attest::attest;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, StructOpt)]
#[structopt(name = "ohttp-server", about = "Serve oblivious HTTP requests.")]
struct Args {
    /// The address to bind to.
    #[structopt(default_value = "127.0.0.1:9443")]
    address: SocketAddr,

    /// When creating message/bhttp, use the indeterminate-length form.
    #[structopt(long, short = "n", alias = "indefinite")]
    indeterminate: bool,

    /// Certificate to use for serving.
    #[structopt(long, short = "c", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/server.crt"))]
    certificate: PathBuf,

    /// Key for the certificate to use for serving.
    #[structopt(long, short = "k", default_value = concat!(env!("CARGO_MANIFEST_DIR"), "/server.key"))]
    key: PathBuf,

    /// Target server
    #[structopt(long, short = "t", default_value = "http://127.0.0.1:5678")]
    target: Url,
}

impl Args {
    fn mode(&self) -> Mode {
        if self.indeterminate {
            Mode::IndeterminateLength
        } else {
            Mode::KnownLength
        }
    }
}

async fn generate_reply(
    ohttp_ref: &Arc<Mutex<OhttpServer>>,
    enc_request: &[u8],
    target: Url,
    _mode: Mode,
) -> Res<(Response, ServerResponse)> {
    let ohttp = ohttp_ref.lock().await;
    let (request, server_response) = ohttp.decapsulate(enc_request)?;
    let bin_request = Message::read_bhttp(&mut Cursor::new(&request[..]))?;

    let method: Method = if let Some(method_bytes) = bin_request.control().method() {
        Method::from_bytes(method_bytes)?
    } else {
        Method::GET
    };

    let mut headers = HeaderMap::new();
    for field in bin_request.header().fields() {
        headers.append(
            HeaderName::from_bytes(field.name()).unwrap(),
            HeaderValue::from_bytes(field.value()).unwrap(),
        );
    }

    let mut t = target;
    if let Some(path_bytes) = bin_request.control().path() {
        if let Ok(path_str) = std::str::from_utf8(path_bytes) {
            t.set_path(path_str);
        }
    }

    let client = reqwest::ClientBuilder::new().build()?;
    let response = client
        .request(method, t)
        .headers(headers)
        .body(bin_request.content().to_vec())
        .send()
        .await?
        .error_for_status()?;

    Ok((response, server_response))
}

#[allow(clippy::unused_async)]
async fn score(
    body: warp::hyper::body::Bytes,
    ohttp: Arc<Mutex<OhttpServer>>,
    target: Url,
    mode: Mode,
) -> Result<impl warp::Reply, std::convert::Infallible> {
    match generate_reply(&ohttp, &body[..], target, mode).await {
        Ok((response, mut server_response)) => {
            let response_nonce = server_response.response_nonce();
            let nonce_stream = once(async { response_nonce });

            let chunk_stream = unfold(
                (true, None, response, server_response, mode),
                |(first, chunk, mut response, mut server_response, mode)| async move {
                    let chunk = if first {
                        response.chunk().await.unwrap()
                    } else {
                        chunk
                    };
                    let Some(chunk) = chunk else { return None };
                    println!(
                        "Processing chunk {} {}",
                        first,
                        std::str::from_utf8(&chunk).unwrap()
                    );
                    let mut bin_response = Message::response(StatusCode::OK);
                    bin_response.write_content(chunk);
                    let mut chunked_response = Vec::new();
                    bin_response
                        .write_bhttp(mode, &mut chunked_response)
                        .unwrap();

                    let (next_chunk, last, err) = match response.chunk().await {
                        Ok(Some(c)) => (Some(c), false, None),
                        Ok(None) => (None, true, None),
                        Err(_) => (None, true, Some(ohttp::Error::Truncated)),
                    };

                    if let Some(_) = err {
                        return None;
                    };
                    let enc_response = server_response
                        .encapsulate_chunk(&chunked_response, last)
                        .unwrap();
                    Some((
                        Ok::<Vec<u8>, ohttp::Error>(enc_response),
                        (false, next_chunk, response, server_response, mode),
                    ))
                },
            );

            let stream = nonce_stream.chain(chunk_stream);
            Ok(warp::http::Response::builder()
                .header("Content-Type", "message/ohttp-chunked-res")
                .body(Body::wrap_stream(stream)))
        }
        Err(e) => {
            println!("400 {}", e.to_string());
            if let Ok(oe) = e.downcast::<::ohttp::Error>() {
                Ok(warp::http::Response::builder()
                    .status(422)
                    .body(Body::from(format!("Error: {oe:?}"))))
            } else {
                Ok(warp::http::Response::builder()
                    .status(400)
                    .body(Body::from(&b"Request error"[..])))
            }
        }
    }
}

#[allow(clippy::unused_async)]
async fn discover(config: String) -> Result<impl warp::Reply, std::convert::Infallible> {
    Ok(warp::http::Response::builder()
        .status(200)
        .body(Vec::from(config)))
}

fn with_ohttp(
    ohttp: Arc<Mutex<OhttpServer>>,
) -> impl Filter<Extract = (Arc<Mutex<OhttpServer>>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || Arc::clone(&ohttp))
}

#[tokio::main]
async fn main() -> Res<()> {
    let args = Args::from_args();
    ::ohttp::init();
    env_logger::try_init().unwrap();
    let Some(token) = attest("".as_bytes(), 0xffff) else {panic!("Failed to get MAA token. You must be root to access TPM.")};
    
    println!("Fetched MAA token: {}", String::from_utf8(token).unwrap());

    let config = KeyConfig::new(
        0,
        Kem::P384Sha384,
        vec![
            SymmetricSuite::new(Kdf::HkdfSha384, Aead::Aes256Gcm),
            SymmetricSuite::new(Kdf::HkdfSha256, Aead::Aes128Gcm),
            SymmetricSuite::new(Kdf::HkdfSha256, Aead::ChaCha20Poly1305),
        ],
    )?;

    let ohttp = OhttpServer::new(config)?;
    let config = hex::encode(KeyConfig::encode_list(&[ohttp.config()])?);

    println!("Config: {}", config);
    let mode = args.mode();
    let target = args.target;

    let score = warp::post()
        .and(warp::path::path("score"))
        .and(warp::path::end())
        .and(warp::body::bytes())
        .and(with_ohttp(Arc::new(Mutex::new(ohttp))))
        .and(warp::any().map(move || target.clone()))
        .and(warp::any().map(move || mode))
        .and_then(score);

    let discover = warp::get()
        .and(warp::path("discover"))
        .and(warp::path::end())
        .and(warp::any().map(move || config.clone()))
        .and_then(discover);

    let routes = score.or(discover);

    warp::serve(routes)
        .tls()
        .cert_path(args.certificate)
        .key_path(args.key)
        .run(args.address)
        .await;

    Ok(())
}
