//! `version_check_probe`: whether a realmd enforces `StrictVersionCheck`, and whether our logon
//! proof satisfies it. It runs the handshake twice, differing only in the proof's `crc_hash`:
//! twenty zeros, which a strict realmd refuses with `WOW_FAIL_VERSION_INVALID` (0x09, shown as
//! `AUTH_VERSION_MISMATCH`, "Wrong client version"), and the computed `SHA1(A ‖ H)`
//! ([`auth::version_proof`]). A non-strict server accepts both.
//!
//! With an install directory the computed arm keys that install's own five binaries
//! ([`auth::Executables`]), as the client does, and the report says whether their digest is the
//! stock one; without one it sends the stock client's stored digest.
//!
//! Usage: `cargo run -p benilla-protocol --example version_check_probe -- <host[:port]> <user>
//! <pass> [install-dir]`. The local strict-mode realmd is `127.0.0.1:3725`, the ordinary one 3724.

use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use benilla_protocol::{auth, host_port, AuthReject, AUTH_PORT, CLIENT_BUILD};
use benilla_srp::{NormalizedString, PublicKey, SrpClientChallenge};

/// Which `crc_hash` the arm puts in the proof packet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// Twenty zeros, forced by handing the writer a `crc_salt` we hold no digest for.
    Zeros,
    /// `SHA1(A ‖ H)` for the salt the server sent.
    Computed,
}

impl Arm {
    fn label(self) -> &'static str {
        match self {
            Arm::Zeros => "zeros   ",
            Arm::Computed => "computed",
        }
    }
}

/// One full handshake with `arm`'s `crc_hash`: `None` when accepted, else the `WOW_FAIL_*` byte.
fn attempt(
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
    arm: Arm,
    own: Option<&auth::Executables>,
) -> Result<Option<u8>> {
    let user_n = NormalizedString::new(user).map_err(|e| anyhow!("invalid username: {e}"))?;
    let pass_n = NormalizedString::new(pass).map_err(|e| anyhow!("invalid password: {e}"))?;

    // Redial past a width-unstable `B`, as `logon` does, so a 0x04 here is a bad password and
    // never the SHA-1 encoding disagreement.
    for _ in 0..8 {
        let mut stream = TcpStream::connect((host, port))
            .with_context(|| format!("connecting to {host}:{port}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        auth::write_logon_challenge(
            &mut stream,
            &user.to_uppercase(),
            auth::ClientVersion::STOCK,
            auth::LOCALE_TAG_EN_US,
        )
        .context("sending logon challenge")?;
        let reply = auth::read_challenge_reply(&mut stream)
            .context("reading challenge reply")?
            .ok_or_else(|| anyhow!("SUCCESS_SURVEY on the challenge: no SRP inputs"))?;
        let b = PublicKey::from_le_bytes(reply.server_public_key)
            .map_err(|e| anyhow!("invalid server public key: {e}"))?;
        if !b.is_width_stable() {
            continue;
        }

        let challenge = SrpClientChallenge::new(
            user_n,
            pass_n,
            reply.generator,
            reply.large_safe_prime,
            b,
            reply.salt,
        );
        let (salt, own) = match arm {
            Arm::Computed => (reply.crc_salt, own),
            Arm::Zeros => ([0u8; 16], None),
        };
        auth::write_logon_proof(
            &mut stream,
            challenge.client_public_key(),
            challenge.client_proof(),
            &salt,
            own,
            None,
        )
        .context("sending logon proof")?;

        return match auth::read_proof_reply(&mut stream) {
            Ok(_) => Ok(None),
            Err(e) => match e.downcast_ref::<AuthReject>() {
                Some(r) => Ok(Some(r.code)),
                None => Err(e),
            },
        };
    }
    Err(anyhow!("eight dials, every `B` ambiguous — try again"))
}

/// What the install's five binaries are, and whether their digest under `crc_salt` is the stock
/// client's, the one sent for an install without them.
fn report_install(install_dir: &std::path::Path, crc_salt: &[u8; 16]) {
    println!("\n  install {}", install_dir.display());
    for name in auth::CHECKSUMMED_EXECUTABLES {
        match std::fs::metadata(install_dir.join(name)) {
            Ok(m) => println!("    {name:<12} {} bytes", m.len()),
            Err(e) => println!("    {name:<12} UNREADABLE ({e})"),
        }
    }
    let Some(own) = auth::Executables::read(install_dir) else {
        println!("    → not all five are readable: the logon sends the stock client's digest");
        return;
    };
    let hex: String = own
        .digest(crc_salt)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    println!("    H = {hex}");
    // Compare through `SHA1(A ‖ H)`, what we send: equal proofs for one `A` mean equal `H`.
    let a = [0u8; 32];
    let stock =
        auth::version_proof(crc_salt, &a, Some(&own)) == auth::version_proof(crc_salt, &a, None);
    println!(
        "    → {}",
        if stock {
            "the stock client's digest for the OS we declare"
        } else {
            "not the stock digest: one of the five is patched, or we declare OSX (then the stock \
             Mac digest goes instead), and a server that keeps only the stock digest refuses it"
        }
    );
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (host_arg, user, pass, install) = match args.as_slice() {
        [h, u, p] => (h.clone(), u.clone(), p.clone(), None),
        [h, u, p, i] => (h.clone(), u.clone(), p.clone(), Some(PathBuf::from(i))),
        _ => {
            eprintln!("usage: version_check_probe <host[:port]> <user> <pass> [install-dir]");
            std::process::exit(2);
        }
    };
    let (host, port) = host_port(&host_arg, AUTH_PORT);

    // Without an install, the stock digest holds only for the one salt every mangos-family realmd
    // sends; under any other, the computed arm sends zeros too.
    let mut stream =
        TcpStream::connect((host, port)).with_context(|| format!("connecting to {host}:{port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    auth::write_logon_challenge(
        &mut stream,
        &user.to_uppercase(),
        auth::ClientVersion::STOCK,
        auth::LOCALE_TAG_EN_US,
    )?;
    let crc_salt = auth::read_challenge_reply(&mut stream)?
        .ok_or_else(|| anyhow!("SUCCESS_SURVEY on the challenge: no SRP inputs"))?
        .crc_salt;
    drop(stream); // no proof follows, so realmd records nothing for this dial
    let hex: String = crc_salt.iter().map(|b| format!("{b:02x}")).collect();
    let own = install.as_deref().and_then(auth::Executables::read);
    let known = auth::version_proof(&crc_salt, &[0u8; 32], own.as_ref()) != [0u8; 20];
    println!("{host}:{port} — build {CLIENT_BUILD}, crc_salt {hex}");
    println!(
        "  we {} an integrity digest for that challenge\n",
        if known { "HOLD" } else { "hold NO" }
    );

    for arm in [Arm::Zeros, Arm::Computed] {
        let verdict = match attempt(host, port, &user, &pass, arm, own.as_ref())? {
            None => "ACCEPTED".to_string(),
            Some(0x09) => {
                "REJECTED 0x09 WOW_FAIL_VERSION_INVALID (\"Wrong client version\")".into()
            }
            Some(c) => format!("REJECTED {c:#04x}"),
        };
        println!("  crc_hash {} → {verdict}", arm.label());
    }
    if let Some(dir) = install {
        report_install(&dir, &crc_salt);
    }
    Ok(())
}
