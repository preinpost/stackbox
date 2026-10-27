//! The daemon reaches this machine's host by running its own binary as `farm agent`.
use farm::remote::{
    link::{AgentLink, ProcessLink},
    protocol::{Operation, Reply, Request, VERSION},
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
