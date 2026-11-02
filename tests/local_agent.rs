//! The daemon reaches this machine's host by running its own binary as `farm agent`.
use farm::remote::{
    link::{AgentLink, ProcessLink},
    protocol::{Frame, Operation, Reply, Request, VERSION},
};

#[tokio::test]
async fn the_farm_binary_answers_as_the_local_agent() {
    let link = ProcessLink {
        program: env!("CARGO_BIN_EXE_farm").into(),
    };
    let reply = link
        .call(
            Request {
                version: VERSION,
                coordinator_id: uuid::Uuid::new_v4().to_string(),
                operation: Operation::Hello,
            },
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(reply, Reply::Hello { version: VERSION, ref build_id, .. } if build_id == farm::BUILD_ID)
    );
    let refused = link
        .call(
            Request {
                version: VERSION + 1,
                coordinator_id: uuid::Uuid::new_v4().to_string(),
                operation: Operation::Hello,
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("incompatible"));
}

/// Deploy jobs live under the agent's home; the binary follows and runs them from there.
#[test]
fn the_farm_binary_looks_for_jobs_under_its_own_home() {
    use std::{io::Write, process::Stdio};
    let home = tempfile::tempdir().unwrap();
    let job_id = uuid::Uuid::new_v4().to_string();
    let mut agent = std::process::Command::new(env!("CARGO_BIN_EXE_farm"))
        .arg("agent")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let request = Request {
        version: VERSION,
        coordinator_id: uuid::Uuid::new_v4().to_string(),
        operation: Operation::Attach {
            job_id: job_id.clone(),
            offset: 0,
        },
    };
    agent
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let output = agent.wait_with_output().unwrap();
    assert!(!output.status.success());
    let frame: Frame = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        matches!(&frame, Frame::Error { message } if message.contains("unknown on this host")),
        "{frame:?}"
    );
    let job_dir = home
        .path()
        .join(".local/share/farm/worker-v1/jobs")
        .join(&job_id);
    let worker = std::process::Command::new(env!("CARGO_BIN_EXE_farm"))
        .args(["agent", "worker"])
        .arg(&job_dir)
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!worker.status.success());
    assert!(String::from_utf8_lossy(&worker.stderr).contains("unknown on this host"));
}
