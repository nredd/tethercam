//! Real codec processing, bounded queues, source cancellation and worker failure tests.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

#[test]
fn rtp_identity_uses_system_entropy_without_identifier_collisions_or_fallbacks() {
    let (configs, timestamp) = stream_configs(&mut |bytes| {
        for (byte, value) in bytes.iter_mut().zip(1_u8..) {
            *byte = value;
        }
        Ok(())
    })
    .unwrap();
    let [main, jpeg_stream] = configs;
    assert_eq!(main.ssrc, 0x0102_0304);
    assert_eq!(jpeg_stream.ssrc, 0x0102_0305);
    assert_eq!(
        (main.initial_sequence, jpeg_stream.initial_sequence),
        (0x0506, 0x0708)
    );
    assert_eq!(timestamp, 0x090a_0b0c);
    assert!(stream_configs(&mut |_| Err(getrandom::Error::UNSUPPORTED)).is_err());
    let (_actor, control) = Supervisor::new(
        Factory::new(Vec::new()),
        SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    let settings = Streams {
        frame_rate: 15,
        bitrate: 100_000,
        h264: true,
        mjpeg: true,
    };
    assert!(
        Pipeline::prepare_with_streams(
            &jpeg(32, 32),
            &settings,
            control.clone(),
            Err(PipelineError::Random)
        )
        .is_err()
    );
    let mut bad = configs;
    bad[0].mtu = 0;
    assert!(
        Pipeline::prepare_with_streams(
            &jpeg(32, 32),
            &settings,
            control.clone(),
            Ok((bad, timestamp))
        )
        .is_err()
    );
    let mut bad = configs;
    bad[1].mtu = 0;
    assert!(
        Pipeline::prepare_with_streams(&jpeg(32, 32), &settings, control, Ok((bad, timestamp)))
            .is_err()
    );
}

#[tokio::test]
async fn input_updates_and_timer_underruns_emit_real_constant_rate_frames() {
    let (mut pipeline, [main, _]) = prepared(true);
    let (tx, rx) = watch::channel(Some(Arc::from(jpeg(32, 32))));
    pipeline.frames = rx;
    main.on_first_subscriber();
    let control = pipeline.control.clone();
    let (result, ()) = tokio::join!(pipeline.run(), async {
        let first = main.next_packet().await.unwrap();
        let start = crate::rtp::Header::decode(first.as_bytes())
            .unwrap()
            .timestamp;
        // Updating before the next slot is due must not emit a burst or busy-spin.
        tx.send_replace(Some(Arc::from(jpeg(32, 32))));
        let mut last = start;
        for _ in 0..10 {
            let packet = main.next_packet().await.unwrap();
            let timestamp = crate::rtp::Header::decode(packet.as_bytes())
                .unwrap()
                .timestamp;
            assert!(timestamp.wrapping_sub(last) <= u32::MAX / 2);
            last = timestamp;
        }
        assert!(last.wrapping_sub(start) > 0);
        control.shutdown();
    });
    result.unwrap();
}

fn cancelled_worker(
    processor: Processor, _frame: Arc<[u8]>, _timestamp: u32, _epochs: [u64; 2],
) -> tokio::task::JoinHandle<Processed> {
    drop(processor);
    let handle = tokio::spawn(std::future::pending::<Processed>());
    handle.abort();
    handle
}

fn delayed_worker(
    processor: Processor, frame: Arc<[u8]>, timestamp: u32, epochs: [u64; 2],
) -> tokio::task::JoinHandle<Processed> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        spawn_worker(processor, frame, timestamp, epochs)
            .await
            .unwrap()
    })
}

#[tokio::test]
async fn actual_worker_bad_frame_and_cancelled_join_propagate_to_the_pipeline() {
    for (spawn, frame) in [
        (
            spawn_worker as SpawnWorker,
            Arc::<[u8]>::from(b"bad jpeg".as_slice()),
        ),
        (cancelled_worker as SpawnWorker, Arc::from(jpeg(32, 32))),
    ] {
        let (mut pipeline, [main, _]) = prepared(true);
        let (_tx, rx) = watch::channel(Some(frame));
        pipeline.frames = rx;
        main.on_first_subscriber();
        assert!(pipeline.run_with_worker(spawn).await.is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn shutdown_joins_the_in_flight_real_codec_job() {
    let (mut pipeline, [main, _]) = prepared(true);
    let (_tx, rx) = watch::channel(Some(Arc::from(jpeg(32, 32))));
    pipeline.frames = rx;
    main.on_first_subscriber();
    let control = pipeline.control.clone();
    let started = tokio::time::Instant::now();
    let (result, ()) = tokio::join!(pipeline.run_with_worker(delayed_worker), async {
        tokio::time::sleep(Duration::from_millis(1)).await;
        control.shutdown();
    });
    result.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(
        crate::rtp::Header::decode(main.next_packet().await.unwrap().as_bytes())
            .unwrap()
            .payload_type,
        96
    );
}

use super::*;
use crate::ptp::session::SessionConfig;
use crate::sony::SonyConfig;
use crate::supervisor::{
    Supervisor,
    testing::{Factory, jpeg},
};

fn prepared(h264: bool) -> (Pipeline, [Arc<RuntimeSource>; 2]) {
    let (_actor, control) = Supervisor::new(
        Factory::new(Vec::new()),
        SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    Pipeline::prepare(
        &jpeg(32, 32),
        &Streams {
            frame_rate: 15,
            bitrate: 100_000,
            h264,
            mjpeg: true,
        },
        control,
    )
    .unwrap()
}

#[tokio::test]
async fn source_generations_reject_stale_packets_and_cancellation_is_safe() {
    use std::future::Future;
    let (mut pipeline, [main, jpeg_source]) = prepared(true);
    assert_eq!(main.track().codec, "H264");
    assert_eq!(jpeg_source.track().payload_type, 26);
    main.on_first_subscriber();
    jpeg_source.on_first_subscriber();
    pipeline
        .processor
        .process(&jpeg(32, 32), 0, [1, 1])
        .unwrap();
    main.on_last_unsubscribe();
    main.on_first_subscriber();
    pipeline
        .processor
        .process(&jpeg(32, 32), 6000, [2, 1])
        .unwrap();
    let packet = main.next_packet().await.unwrap();
    assert_eq!(
        crate::rtp::Header::decode(packet.as_bytes())
            .unwrap()
            .timestamp,
        6000
    );
    jpeg_source.on_last_unsubscribe();
    main.epoch.store(u64::MAX, Ordering::Relaxed);
    main.on_first_subscriber();
    assert_eq!(main.epoch.load(Ordering::Relaxed), 1);
    assert_eq!(*main.control.demand.borrow(), [1, 0]);
    while main.rx.lock().await.try_recv().is_ok() {}
    {
        let pending = main.next_packet();
        tokio::pin!(pending);
        std::future::poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }
    drop(pipeline);
    assert_eq!(main.next_packet().await, None);
}

#[tokio::test]
async fn per_frame_paths_reject_bad_jpeg_and_size_changes_and_drop_on_full() {
    let (mut pipeline, sources) = prepared(true);
    let processor = &mut pipeline.processor;
    processor.process(&jpeg(32, 32), 0, [0, 0]).unwrap();
    processor.process(&jpeg(32, 32), 0, [1, 1]).unwrap();
    processor.process(&jpeg(32, 32), 6000, [1, 1]).unwrap();
    assert!(processor.process(b"bad jpeg", 12000, [1, 0]).is_err());
    assert!(processor.process(&jpeg(64, 64), 12000, [1, 0]).is_err());
    assert!(processor.process(b"bad jpeg", 12000, [0, 1]).is_err());
    assert!(processor.process(&jpeg(64, 64), 12000, [0, 1]).is_err());
    // Hundreds of real encoded frames fill the bounded queue, never an unbounded fixture list.
    for timestamp in 0..600 {
        processor.process(&jpeg(32, 32), timestamp, [1, 1]).unwrap();
    }
    assert_eq!(sources[0].rx.lock().await.len(), 512);
    assert_eq!(sources[1].rx.lock().await.len(), 512);
    drop(sources);
    processor.process(&jpeg(32, 32), 0, [1, 1]).unwrap(); // closed receivers are nonblocking too
    let (mut jpeg_only, _) = prepared(false);
    jpeg_only
        .processor
        .process(&jpeg(32, 32), 0, [1, 0])
        .unwrap();
}

#[tokio::test]
async fn invalid_media_and_encoder_configuration_and_worker_failure_are_typed() {
    let (_actor, control) = Supervisor::new(
        Factory::new(Vec::new()),
        SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    let mut settings = Streams {
        frame_rate: 0,
        bitrate: 100_000,
        h264: true,
        mjpeg: true,
    };
    assert!(Pipeline::prepare(b"bad", &settings, control.clone()).is_err());
    settings.mjpeg = false;
    assert!(Pipeline::prepare(b"bad", &settings, control.clone()).is_err());
    settings.mjpeg = true;
    assert!(Pipeline::prepare(&jpeg(32, 32), &settings, control.clone()).is_err());
    settings.h264 = false;
    let (pipeline, _) = Pipeline::prepare(&jpeg(32, 32), &settings, control).unwrap();
    assert!(pipeline.run().await.is_err());
    let handle = tokio::spawn(std::future::pending::<(Processor, Result<(), PipelineError>)>());
    handle.abort();
    let result = worker_result(handle.await);
    assert_eq!(
        std::mem::discriminant(&result.err().unwrap()),
        std::mem::discriminant(&PipelineError::Worker)
    );
    let (pipeline, _) = prepared(true);
    pipeline.run().await.unwrap(); // a closed camera frame channel is EOF, not a busy loop
}
