//! "SDK floor" probe for the optimization review (experiment E8; bench only).
//!
//! Issues `count` GETs or PUTs of `size`-byte objects with `inflight` requests in flight, through
//!   sdk            aws-sdk-s3, configured as rs5cmd's default path configures it
//!   lean           a hyper 1 client (HTTP/1.1 pool) with the same SigV4 signing as `--fast`
//!   lean-unsigned  the same hyper client without signing
//! Run it under bench/review/launch against s3stub to compare CPU time and peak RSS: the gap
//! between `sdk` and `lean` is what the SDK's request pipeline costs on top of HTTP + SigV4.
//!
//!   sdkfloor <sdk|lean|lean-unsigned> <get|put> <endpoint> <bucket> <prefix> <count> <size> <inflight>
//!   cargo build --release --features "bench-stub fast" --bin sdkfloor

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 9 {
        anyhow::bail!("usage: sdkfloor <sdk|lean|lean-unsigned> <get|put> <endpoint> <bucket> <prefix> <count> <size> <inflight>");
    }
    let (mode, op, ep, bucket, prefix) = (a[1].clone(), a[2].clone(), a[3].clone(), a[4].clone(), a[5].clone());
    let count: usize = a[6].parse()?;
    let size: usize = a[7].parse()?;
    let inflight: usize = a[8].parse()?;
    let put = op == "put";
    let payload = Bytes::from(vec![b'x'; size]);
    let key = move |i: usize| format!("{prefix}obj_{i:07}.dat");

    let mut set = tokio::task::JoinSet::new();
    let mut done = 0usize;
    let mut bytes = 0u64;
    match mode.as_str() {
        "sdk" => {
            use aws_sdk_s3::config::{Region, RequestChecksumCalculation, ResponseChecksumValidation};
            let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .load()
                .await;
            let conf = aws_sdk_s3::config::Builder::from(&shared)
                .endpoint_url(&ep)
                .force_path_style(true)
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
                .build();
            let client = aws_sdk_s3::Client::from_conf(conf);
            for i in 0..count {
                while set.len() >= inflight {
                    bytes += set.join_next().await.unwrap()??;
                    done += 1;
                }
                let (c, b, k, p) = (client.clone(), bucket.clone(), key(i), payload.clone());
                set.spawn(async move {
                    if put {
                        c.put_object().bucket(b).key(k).body(p.into()).send().await?;
                        anyhow::Ok(size as u64)
                    } else {
                        let r = c.get_object().bucket(b).key(k).send().await?;
                        Ok(r.body.collect().await?.into_bytes().len() as u64)
                    }
                });
            }
        }
        "lean" | "lean-unsigned" => {
            use hyper_util::client::legacy::Client;
            use hyper_util::rt::TokioExecutor;
            let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new())
                .pool_max_idle_per_host(usize::MAX)
                .build_http();
            let signer = (mode == "lean").then(|| {
                Arc::new(rs5cmd::fastpath::sign::Signer::new(
                    std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
                    std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
                    None,
                    "us-east-1".into(),
                ))
            });
            let host = ep.split("://").nth(1).unwrap_or(&ep).trim_end_matches('/').to_string();
            for i in 0..count {
                while set.len() >= inflight {
                    bytes += set.join_next().await.unwrap()??;
                    done += 1;
                }
                let (c, s, p) = (client.clone(), signer.clone(), payload.clone());
                let uri = format!("{}/{}/{}", ep.trim_end_matches('/'), bucket, key(i));
                let host = host.clone();
                set.spawn(async move {
                    let mut req = http::Request::builder()
                        .method(if put { "PUT" } else { "GET" })
                        .uri(&uri)
                        .header("host", host)
                        .body(())?;
                    if put {
                        req.headers_mut().insert("content-length", p.len().into());
                    }
                    if let Some(s) = &s {
                        s.sign_s3(&mut req, &[])?;
                    }
                    let (parts, ()) = req.into_parts();
                    let body = if put { Full::new(p) } else { Full::new(Bytes::new()) };
                    let resp = c.request(http::Request::from_parts(parts, body)).await?;
                    let status = resp.status();
                    let n = resp.into_body().collect().await?.to_bytes().len() as u64;
                    anyhow::ensure!(status.is_success(), "HTTP {status}");
                    anyhow::Ok(if put { size as u64 } else { n })
                });
            }
        }
        m => anyhow::bail!("unknown mode {m}"),
    }
    while let Some(r) = set.join_next().await {
        bytes += r??;
        done += 1;
    }
    println!("{mode} {op}: {done} requests, {bytes} bytes");
    Ok(())
}
