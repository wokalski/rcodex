use anyhow::{Context, Result, ensure};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use std::{fs, net::TcpStream, path::Path, sync::Arc, time::Duration};

// The certificate reaches the client through authenticated SSH, not the network
// endpoint being authenticated. Private keys never leave the remote host.
pub fn check(host: &str, port: u16, certificate: &str) -> Result<()> {
    use std::net::ToSocketAddrs;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_slice(certificate.as_bytes())?)?;
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut last = None;
    for address in (host, port).to_socket_addrs()? {
        let result = (|| -> Result<()> {
            let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
            socket.set_read_timeout(Some(Duration::from_secs(3)))?;
            socket.set_write_timeout(Some(Duration::from_secs(3)))?;
            let mut tls = rustls::ClientConnection::new(
                Arc::new(config.clone()),
                ServerName::try_from(host.to_owned())?,
            )?;
            while tls.is_handshaking() {
                tls.complete_io(&mut socket)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("hostname resolved to no addresses")))
}

pub fn write_roots(path: &Path, certificate: &str) -> Result<()> {
    // SSL_CERT_FILE replaces native roots, so retain those too for other Codex
    // connections. This file lives only as long as the local client does.
    let mut bundle = String::new();
    for cert in rustls_native_certs::load_native_certs().certs {
        bundle.push_str(&pem::encode(&pem::Pem::new("CERTIFICATE", cert.as_ref())));
    }
    bundle.push_str(certificate);
    fs::write(path, bundle)?;
    Ok(())
}

pub fn serve(port: u16, token: &str, certificate: &str, key: &str) -> Result<()> {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from_pem_file(certificate)?],
            PrivateKeyDer::from_pem_file(key)?,
        )?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async {
            let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
            let backend = reservation.local_addr()?;
            drop(reservation);
            let mut child = tokio::process::Command::new("codex")
                .args([
                    "app-server", "--listen", &format!("ws://{backend}"),
                    "--ws-auth", "capability-token", "--ws-token-file", token,
                ])
                .kill_on_drop(true)
                .spawn()
                .context("launch Codex behind TLS")?;
            let mut ready = false;
            for _ in 0..150 {
                ensure!(child.try_wait()?.is_none(), "Codex exited before becoming ready");
                if tokio::net::TcpStream::connect(backend).await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            ensure!(ready, "Codex did not become ready");
            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
            let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
            loop {
                tokio::select! {
                    status = child.wait() => anyhow::bail!("Codex exited: {}", status?),
                    accepted = listener.accept() => {
                        let (socket, _) = accepted?;
                        let acceptor = acceptor.clone();
                        tokio::spawn(async move {
                            let Ok(Ok(mut tls)) = tokio::time::timeout(
                                Duration::from_secs(10), acceptor.accept(socket),
                            ).await else { return; };
                            if let Ok(mut upstream) = tokio::net::TcpStream::connect(backend).await {
                                let _ = tokio::io::copy_bidirectional(&mut tls, &mut upstream).await;
                            }
                        });
                    }
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_requires_the_ssh_certificate_and_matching_hostname() {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let certificate = cert.cert.pem();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
            )
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let config = Arc::new(config);
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut tls = rustls::ServerConnection::new(config.clone()).unwrap();
                let _ = tls.complete_io(&mut socket);
            }
        });
        assert!(check("127.0.0.1", port, &certificate).is_ok());
        assert!(check("localhost", port, &certificate).is_err());
        let other = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        assert!(check("127.0.0.1", port, &other.cert.pem()).is_err());
        server.join().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let roots = directory.path().join("roots.pem");
        write_roots(&roots, &certificate).unwrap();
        assert!(fs::read_to_string(roots).unwrap().contains(&certificate));
    }
}
