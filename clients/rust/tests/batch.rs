//! Parallel sessions and `StepBatch` against a real `dust-headless` (see `boot.rs` for what it
//! needs).

mod common;

use dust_client::{pb, Action, Batch, Button, Cpu, ImageFormat, MemRange, ObservationSpec};
use sha1::{Digest, Sha1};

const MAIN_RAM: u32 = 0x0200_0000;

async fn batch(count: usize) -> Batch {
    let first = common::launch().await;
    let mut sessions = vec![first.clone()];
    let rest = Batch::create(
        first.client(),
        common::test_rom(),
        &dust_client::SessionOptions {
            rtc_unix_time: Some(1_700_000_000),
            ..Default::default()
        },
        count - 1,
    )
    .await
    .unwrap();
    sessions.extend(rest.sessions().iter().cloned());
    Batch::new(first.client().clone(), sessions)
}

#[tokio::test(flavor = "multi_thread")]
async fn identical_inputs_give_identical_ram_at_frame_600() {
    let batch = batch(8).await;
    let pattern = [
        Action::default(),
        Action::buttons(&[Button::A]),
        Action::buttons(&[Button::Start, Button::Down]),
        Action {
            buttons: vec![Button::B],
            touch: Some((100, 50)),
        },
    ];
    for step in 0..60 {
        let observations = batch
            .step_all(&pattern[step % pattern.len()], 10)
            .await
            .unwrap();
        assert_eq!(observations.len(), 8);
        assert!(observations
            .iter()
            .all(|observation| observation.status.frame == (step as u64 + 1) * 10));
    }

    let mut hashes = Vec::new();
    for session in batch.sessions() {
        assert_eq!(session.status().await.unwrap().frame, 600);
        let ram = session.read(Cpu::Arm9, MAIN_RAM, 4 << 20).await.unwrap();
        hashes.push(Sha1::digest(&ram));
    }
    assert!(hashes.iter().all(|hash| *hash == hashes[0]), "{hashes:x?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn observations_follow_the_spec() {
    let batch = batch(2).await.with_observation(ObservationSpec {
        screen: true,
        grayscale: true,
        downscale: 4,
        ram: vec![
            MemRange {
                addr: MAIN_RAM,
                len: 16,
            },
            MemRange {
                addr: MAIN_RAM + 0x100,
                len: 4,
            },
        ],
        variables: false,
    });
    let actions = [Action::buttons(&[Button::A]), Action::default()];
    let observations = batch.step(&actions, 0).await.unwrap();
    for (observation, session) in observations.iter().zip(batch.sessions()) {
        // frames = 0 steps one frame.
        assert_eq!(observation.status.frame, 1);
        let screen = observation.screen.as_ref().unwrap();
        assert_eq!((screen.width, screen.height), (64, 96));
        assert_eq!(screen.format(), ImageFormat::RawGray);
        assert_eq!(screen.data.len(), 64 * 96);
        assert_eq!(observation.ram.len(), 2);
        assert_eq!(
            observation.ram[0],
            session.read(Cpu::Arm9, MAIN_RAM, 16).await.unwrap()
        );
        assert_eq!(observation.ram[1].len(), 4);
    }

    let rgb = batch.clone().with_observation(ObservationSpec {
        screen: true,
        ..Default::default()
    });
    let observations = rgb.step(&actions, 1).await.unwrap();
    let screen = observations[0].screen.as_ref().unwrap();
    assert_eq!((screen.width, screen.height), (256, 384));
    assert_eq!(screen.format(), ImageFormat::RawRgb);
    let full = batch.sessions()[0]
        .screenshot(ImageFormat::RawRgb)
        .await
        .unwrap();
    assert_eq!(screen.data, full.data);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_batches_are_rejected_before_stepping() {
    let batch = batch(2).await;
    let ids: Vec<String> = batch.sessions().iter().map(|s| s.id().to_owned()).collect();
    let item = |session_id: &str, action: pb::Action| pb::BatchItem {
        session_id: session_id.to_owned(),
        action: Some(action),
        frames: 5,
    };
    let call = |items: Vec<pb::BatchItem>| {
        let mut grpc = batch.sessions()[0].client().grpc();
        async move {
            grpc.step_batch(pb::StepBatchRequest {
                items,
                observation: None,
            })
            .await
            .unwrap_err()
            .code()
        }
    };

    let cases = [
        (
            vec![
                item(&ids[0], Default::default()),
                item(&ids[0], Default::default()),
            ],
            tonic::Code::InvalidArgument,
        ),
        (
            vec![
                item(&ids[0], Default::default()),
                item("nope", Default::default()),
            ],
            tonic::Code::NotFound,
        ),
        (
            vec![item(
                &ids[1],
                pb::Action {
                    touch: Some(pb::TouchPoint { x: 256, y: 0 }),
                    ..Default::default()
                },
            )],
            tonic::Code::InvalidArgument,
        ),
        (
            vec![item(
                &ids[1],
                pb::Action {
                    discrete: Some(0),
                    ..Default::default()
                },
            )],
            tonic::Code::FailedPrecondition,
        ),
    ];
    for (items, code) in cases {
        assert_eq!(call(items).await, code);
    }
    for session in batch.sessions() {
        assert_eq!(session.status().await.unwrap().frame, 0);
    }

    let err = batch.step(&[Action::default()], 1).await.unwrap_err();
    assert!(
        matches!(err, dust_client::Error::Rpc(status) if status.code() == tonic::Code::InvalidArgument)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn several_servers_run_side_by_side() {
    let (first, second) = tokio::join!(batch(2), batch(3));
    let (idle, press_a) = (Action::default(), Action::buttons(&[Button::A]));
    let (a, b) = tokio::join!(first.step_all(&idle, 7), second.step_all(&press_a, 9));
    assert!(a.unwrap().iter().all(|o| o.status.frame == 7));
    assert!(b.unwrap().iter().all(|o| o.status.frame == 9));
}
