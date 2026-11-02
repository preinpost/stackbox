//! Real VMs joined by links through a switch on this host. Needs msb, network access for the
//! golden image and iperf3, and a Linux farm-agent for the guests:
//!
//! ```sh
//! sh scripts/build-agent.sh aarch64
//! FARM_GUEST_AGENT=$PWD/dist/agent/aarch64/farm-agent \
//!   cargo test --release --test farm_link -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use farm::farm::{ALL_IN_ONE, NodeSetup, NodeSpec, connect, create_node, delete_farm};
use farm::link::{Impair, guest::LinkConf, host::Switch};

const VMS: [(&str, &str, &str); 3] = [
    // VM, port, segment
    ("link-test-a", "lt/a/m", "lt/m"),
    ("link-test-b", "lt/b/m", "lt/m"),
    ("link-test-c", "lt/c/m", "lu/m"),
];
const DEV: &str = "lt0";

fn address(i: usize) -> String {
    format!("10.253.0.{}", i + 2)
}

async fn sh(vm: &str, script: &str) -> Result<String> {
    let sandbox = connect(vm).await?;
    let out = sandbox.exec("bash", ["-c", script]).await;
    sandbox.detach().await;
    let out = out?;
    ensure!(
        out.status().success,
        "{vm}: {script}: {}",
        out.stderr()?.trim()
    );
    Ok(out.stdout()?)
}

async fn reaches(from: &str, to: usize) -> bool {
    sh(from, &format!("ping -c 2 -W 1 -i 0.2 {}", address(to)))
        .await
        .is_ok()
}

/// Wait up to `seconds` for `from` to reach `to`.
async fn reaches_within(from: &str, to: usize, seconds: u64) -> Result<Duration> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        if reaches(from, to).await {
            return Ok(started.elapsed());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("{from} cannot reach {} after {seconds}s", address(to))
}

async fn iperf(mtu: u16) -> Result<String> {
    for vm in [VMS[0].0, VMS[1].0] {
        sh(vm, &format!("ip link set {DEV} mtu {mtu}")).await?;
    }
    sh(VMS[1].0, "pgrep iperf3 >/dev/null || iperf3 -s -D").await?;
    let out = sh(
        VMS[0].0,
        &format!(
            "iperf3 -c {} -t 5 -f m | grep -E 'sender|receiver'",
            address(1)
        ),
    )
    .await?;
    let ping = sh(
        VMS[0].0,
        &format!("ping -c 50 -i 0.05 {} | tail -1", address(1)),
    )
    .await?;
    Ok(format!("MTU {mtu}:\n{out}{ping}"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots real VMs; see the module comment"]
async fn vms_share_a_segment_through_the_switch() {
    let dir = tempfile::tempdir().unwrap();
    let switch = Switch::new(dir.path());
    // The switch process the agent starts, from this build's `farm`.
    let command: Vec<std::ffi::OsString> = vec![
        env!("CARGO_BIN_EXE_farm").into(),
        "agent".into(),
        "switch".into(),
    ];
    let kill = || {
        if let Ok(pid) = std::fs::read_to_string(dir.path().join("switch/pid")) {
            let _ = std::process::Command::new("kill")
                .args(["-9", pid.trim()])
                .status();
        }
    };
    let result = async {
        let spec = NodeSpec {
            cpus: 2,
            memory_mib: 1024,
            ..ALL_IN_ONE
        };
        switch.ensure(&command).await?;
        let mut tokens = Vec::new();
        for (i, (vm, port, segment)) in VMS.iter().enumerate() {
            let token = switch.add_port(port, segment)?;
            tokens.push(token.clone());
            let link = LinkConf {
                dev: DEV.into(),
                switch: switch.guest_url()?,
                port: (*port).into(),
                token,
                mtu: farm::link::DEFAULT_MTU,
                address: Some(format!("{}/24", address(i))),
            };
            // Boxed: the creation future is too deep to nest in this one.
            let setup = NodeSetup {
                links: vec![link],
                ..Default::default()
            };
            Box::pin(create_node(vm, spec, &setup))
                .await
                .with_context(|| format!("create {vm}"))?;
        }
        let (a, b, c) = (VMS[0].0, VMS[1].0, VMS[2].0);
        println!(
            "{}",
            sh(a, &format!("ip -br addr show {DEV}; ip -o link show {DEV}")).await?
        );
        reaches_within(a, 1, 30).await?;
        ensure!(!reaches(a, 2).await, "a reached c on another segment");
        ensure!(!reaches(c, 0).await, "c reached a on another segment");
        println!("a ⇄ b on one segment; c on another is unreachable");

        for vm in [a, b] {
            sh(
                vm,
                "command -v iperf3 >/dev/null || dnf install -y -q iperf3",
            )
            .await?;
        }
        println!("{}", iperf(9000).await?);
        println!("{}", iperf(1500).await?);

        switch.impair(
            VMS[0].1,
            Impair {
                delay_ms: 0,
                loss: 1.0,
            },
        )?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        ensure!(!reaches(a, 1).await, "isolated a still reaches b");
        switch.impair(
            VMS[0].1,
            Impair {
                delay_ms: 25,
                loss: 0.0,
            },
        )?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        println!(
            "a delayed 25 ms: {}",
            sh(a, &format!("ping -c 5 -i 0.2 {} | tail -1", address(1))).await?
        );
        switch.impair(VMS[0].1, Impair::default())?;
        println!("isolation and delay apply and heal");

        // The address stays while the link process restarts.
        sh(a, &format!("systemctl restart farm-link@{DEV}")).await?;
        println!(
            "link restart: back after {:?}; {}",
            reaches_within(a, 1, 30).await?,
            sh(a, &format!("ip -br addr show {DEV}")).await?.trim()
        );

        // Both links reconnect to a restarted switch.
        kill();
        tokio::time::sleep(Duration::from_secs(2)).await;
        ensure!(!switch.running()?, "the switch survived SIGKILL");
        ensure!(!reaches(a, 1).await, "a reached b without a switch");
        switch.ensure(&command).await?;
        println!(
            "switch restart: back after {:?}",
            reaches_within(a, 1, 30).await?
        );

        // A rotated token shuts the old guest out.
        switch.add_port(VMS[0].1, VMS[0].2)?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        ensure!(!reaches(a, 1).await, "a still links with a revoked token");
        let log = sh(
            a,
            &format!("journalctl -u farm-link@{DEV} --no-pager | tail -3"),
        )
        .await?;
        ensure!(log.contains("refused"), "{log}");
        println!("revoked token refused");

        // A restarted VM brings its link up at boot; it gets a fresh token first.
        let token = switch.add_port(VMS[0].1, VMS[0].2)?;
        sh(
            a,
            &format!("sed -i 's/^TOKEN=.*/TOKEN={token}/' /etc/farm/link-{DEV}.conf"),
        )
        .await?;
        let status = std::process::Command::new("msb")
            .args(["stop", a])
            .status()?;
        ensure!(status.success());
        let status = std::process::Command::new("msb")
            .args(["start", a])
            .status()?;
        ensure!(status.success());
        let sandbox = connect(a).await?;
        farm::farm::wait_until_booted(&sandbox).await?;
        sandbox.detach().await;
        println!(
            "VM restart: back after {:?}",
            reaches_within(a, 1, 60).await?
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    for (vm, _, _) in VMS {
        let _ = delete_farm(vm).await;
    }
    kill();
    println!(
        "switch log:\n{}",
        std::fs::read_to_string(dir.path().join("switch/switch.log")).unwrap_or_default()
    );
    result.unwrap();
}
