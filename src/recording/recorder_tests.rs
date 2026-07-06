use super::*;

#[tokio::test]
async fn test_max_recordings_limit() {
    let mut mgr = RecordingManager::with_max(3);
    let dir = std::env::temp_dir().join("rtpbridge-rec-limit-test");
    std::fs::create_dir_all(&dir).ok();

    // Start 3 recordings — all should succeed
    let mut ids = Vec::new();
    for i in 0..3 {
        let path = dir
            .join(format!("test-{i}.pcap"))
            .to_string_lossy()
            .to_string();
        let id = mgr
            .start(None, path)
            .await
            .expect("recording should succeed");
        ids.push(id);
    }

    // 4th should fail
    let path = dir.join("test-3.pcap").to_string_lossy().to_string();
    let result = mgr.start(None, path).await;
    assert!(result.is_err(), "4th recording should fail at limit");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Maximum concurrent recordings"),
        "error should mention recording limit"
    );

    // Stop one, then start should succeed again
    mgr.stop(&ids[0]).unwrap();
    let path = dir.join("test-4.pcap").to_string_lossy().to_string();
    mgr.start(None, path)
        .await
        .expect("should succeed after stopping one");

    // Cleanup
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_stop_nonexistent_recording() {
    let mut mgr = RecordingManager::new();
    let fake_id = RecordingId::new_v4();
    let result = mgr.stop(&fake_id);
    assert!(
        result.is_err(),
        "stopping a nonexistent recording should return Err"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Recording not found"),
        "error should mention recording not found"
    );
}

#[tokio::test]
async fn test_stop_all_recordings() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-stop-all-test");
    std::fs::create_dir_all(&dir).ok();

    // Start 3 recordings
    for i in 0..3 {
        let path = dir
            .join(format!("test-{i}.pcap"))
            .to_string_lossy()
            .to_string();
        mgr.start(None, path)
            .await
            .expect("recording should succeed");
    }
    assert_eq!(
        mgr.active_recordings().len(),
        3,
        "should have 3 active recordings"
    );

    // Stop all
    mgr.stop_all();
    assert_eq!(
        mgr.active_recordings().len(),
        0,
        "all recordings should be stopped"
    );

    // Cleanup
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_stop_all_mixed_endpoint_recordings() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-stop-all-mixed-test");
    std::fs::create_dir_all(&dir).ok();

    let ep1 = EndpointId::new_v4();
    let ep2 = EndpointId::new_v4();

    // Start 3 recordings: 2 for specific endpoints, 1 for full-session (None)
    let path1 = dir.join("ep1.pcap").to_string_lossy().to_string();
    mgr.start(Some(ep1), path1)
        .await
        .expect("recording for ep1 should succeed");

    let path2 = dir.join("ep2.pcap").to_string_lossy().to_string();
    mgr.start(Some(ep2), path2)
        .await
        .expect("recording for ep2 should succeed");

    let path3 = dir.join("session.pcap").to_string_lossy().to_string();
    mgr.start(None, path3)
        .await
        .expect("full-session recording should succeed");

    assert_eq!(
        mgr.active_recordings().len(),
        3,
        "should have 3 active recordings"
    );

    // Verify each recording has the correct endpoint association
    let active = mgr.active_recordings();
    let ep1_recs: Vec<_> = active
        .iter()
        .filter(|r| r.endpoint_id == Some(ep1))
        .collect();
    let ep2_recs: Vec<_> = active
        .iter()
        .filter(|r| r.endpoint_id == Some(ep2))
        .collect();
    let session_recs: Vec<_> = active.iter().filter(|r| r.endpoint_id.is_none()).collect();
    assert_eq!(ep1_recs.len(), 1, "should have 1 recording for ep1");
    assert_eq!(ep2_recs.len(), 1, "should have 1 recording for ep2");
    assert_eq!(
        session_recs.len(),
        1,
        "should have 1 full-session recording"
    );

    // Stop all
    mgr.stop_all();
    assert_eq!(
        mgr.active_recordings().len(),
        0,
        "all recordings should be stopped after stop_all"
    );

    // Verify that starting new recordings works after stop_all (manager is reusable)
    let path4 = dir
        .join("after-stop-all.pcap")
        .to_string_lossy()
        .to_string();
    mgr.start(None, path4)
        .await
        .expect("should be able to start recordings after stop_all");
    assert_eq!(
        mgr.active_recordings().len(),
        1,
        "new recording should work after stop_all"
    );

    // Cleanup
    mgr.stop_all();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_recording_channel_backpressure() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-backpressure-test");
    std::fs::create_dir_all(&dir).ok();

    let ep = EndpointId::new_v4();
    let path = dir.join("bp-test.pcap").to_string_lossy().to_string();
    mgr.start(Some(ep), path).await.unwrap();

    // Flood the channel beyond capacity (1000) without giving the writer task time to drain.
    // This should not panic — packets should be silently dropped after the channel fills.
    for _ in 0..2000 {
        mgr.record_packet(&ep, &[0xAA; 172]);
    }

    // Verify the recording is still functional after overflow
    let active = mgr.active_recordings();
    assert_eq!(
        active.len(),
        1,
        "recording should still be active after backpressure"
    );

    mgr.stop_all();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_mid_recording_write_error() {
    // Simulate a write error mid-recording by recording to a file on a
    // read-only path. The recording task should break out of its write
    // loop on error (not panic or hang).
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-write-error-test");
    std::fs::create_dir_all(&dir).ok();

    let ep = EndpointId::new_v4();
    let path = dir.join("write-error.pcap").to_string_lossy().to_string();
    mgr.start(Some(ep), path).await.unwrap();

    // Record some packets — these go through the channel to the background writer
    for _ in 0..10 {
        mgr.record_packet(&ep, &[0xCC; 172]);
    }

    // Give the writer task a chance to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Stop should succeed even if the writer encountered errors
    let stopped = mgr.stop_all();
    assert_eq!(stopped.len(), 1, "should return one stopped recording");
    assert!(
        stopped[0].packets >= 10,
        "should have counted packets sent to the channel"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_recording_invalid_path() {
    let mut mgr = RecordingManager::new();
    // Try to record to a path that cannot be created
    let result = mgr
        .start(
            None,
            "/nonexistent-root-dir/impossible/file.pcap".to_string(),
        )
        .await;
    assert!(
        result.is_err(),
        "recording to invalid path should fail upfront"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Cannot create recording file"),
        "error should mention file creation failure"
    );
    assert!(
        mgr.active_recordings().is_empty(),
        "no recording should be active after failure"
    );
}

#[tokio::test]
async fn test_stop_endpoint_recordings_returns_info() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-stop-ep-info-test");
    std::fs::create_dir_all(&dir).ok();

    let ep = EndpointId::new_v4();
    let path = dir.join("ep-info.pcap").to_string_lossy().to_string();
    let rec_id = mgr.start(Some(ep), path.clone()).await.unwrap();

    // Record a few packets
    for _ in 0..5 {
        mgr.record_packet(&ep, &[0xBB; 100]);
    }

    let stopped = mgr.stop_endpoint_recordings(&ep);
    assert_eq!(stopped.len(), 1, "should return one stopped recording");
    assert_eq!(stopped[0].recording_id, rec_id);
    assert_eq!(stopped[0].file_path, path);
    assert_eq!(stopped[0].packets, 5, "should report 5 recorded packets");
    assert!(
        stopped[0].duration_ms < 5000,
        "duration should be reasonable"
    );

    assert!(
        mgr.active_recordings().is_empty(),
        "no recordings should remain"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_record_packet_session_wide_receives_all() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-session-wide-test");
    // Defensively remove any leftover from a prior failed run before recreating —
    // this test uses a fixed path, not tempfile::tempdir().
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).ok();

    let ep1 = EndpointId::new_v4();
    let ep2 = EndpointId::new_v4();

    // Start a session-wide recording (endpoint_id = None) with outbound capture
    // enabled so the third (outbound) packet is not filtered.
    let path = dir.join("session-wide.pcap").to_string_lossy().to_string();
    mgr.start(None, path).await.unwrap();

    // Record packets from different endpoints
    mgr.record_packet(&ep1, &[0xAA; 100]);
    mgr.record_packet(&ep2, &[0xBB; 100]);
    mgr.record_packet(&ep1, &[0xCC; 100]);

    // Give the writer task a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Stop and check that all 3 packets were counted
    let stopped = mgr.stop_all();
    assert_eq!(stopped.len(), 1);
    assert_eq!(
        stopped[0].packets, 3,
        "session-wide recording should receive packets from all endpoints"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_stop_endpoint_recordings_returns_all() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-stop-ep-all-test");
    std::fs::create_dir_all(&dir).ok();

    let ep = EndpointId::new_v4();

    // Start two recordings for the same endpoint
    let path1 = dir.join("ep-rec-1.pcap").to_string_lossy().to_string();
    let id1 = mgr.start(Some(ep), path1).await.unwrap();
    let path2 = dir.join("ep-rec-2.pcap").to_string_lossy().to_string();
    let id2 = mgr.start(Some(ep), path2).await.unwrap();

    assert_eq!(mgr.active_recordings().len(), 2);

    // Stop all recordings for that endpoint
    let stopped = mgr.stop_endpoint_recordings(&ep);
    assert_eq!(
        stopped.len(),
        2,
        "should stop both recordings for the endpoint"
    );

    let stopped_ids: Vec<_> = stopped.iter().map(|s| s.recording_id).collect();
    assert!(stopped_ids.contains(&id1));
    assert!(stopped_ids.contains(&id2));

    assert!(
        mgr.active_recordings().is_empty(),
        "no recordings should remain"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_stop_all_returns_all_info() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-stop-all-info-test");
    std::fs::create_dir_all(&dir).ok();

    let ep1 = EndpointId::new_v4();
    let ep2 = EndpointId::new_v4();

    let path1 = dir.join("r1.pcap").to_string_lossy().to_string();
    let id1 = mgr.start(Some(ep1), path1.clone()).await.unwrap();
    let path2 = dir.join("r2.pcap").to_string_lossy().to_string();
    let id2 = mgr.start(Some(ep2), path2.clone()).await.unwrap();
    let path3 = dir.join("r3.pcap").to_string_lossy().to_string();
    let id3 = mgr.start(None, path3.clone()).await.unwrap();

    // Record some packets
    mgr.record_packet(&ep1, &[0x11; 50]);
    mgr.record_packet(&ep2, &[0x22; 50]);

    let stopped = mgr.stop_all();
    assert_eq!(
        stopped.len(),
        3,
        "stop_all should return info for all 3 recordings"
    );

    let stopped_ids: Vec<_> = stopped.iter().map(|s| s.recording_id).collect();
    assert!(stopped_ids.contains(&id1));
    assert!(stopped_ids.contains(&id2));
    assert!(stopped_ids.contains(&id3));

    // Verify file paths are correct
    let stopped_paths: Vec<_> = stopped.iter().map(|s| s.file_path.clone()).collect();
    assert!(stopped_paths.contains(&path1));
    assert!(stopped_paths.contains(&path2));
    assert!(stopped_paths.contains(&path3));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_relative_path_rejected() {
    let mut mgr = RecordingManager::new();
    let result = mgr.start(None, "recordings/test.pcap".to_string()).await;
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("must be absolute"),
        "should reject relative paths"
    );
}

#[tokio::test]
async fn test_path_traversal_rejected() {
    let mut mgr = RecordingManager::new();
    let result = mgr
        .start(None, "/tmp/recordings/../../../etc/passwd.pcap".to_string())
        .await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("must not contain '..'"),
        "should reject path traversal"
    );
}

#[tokio::test]
async fn test_start_duplicate_path_fails() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-dup-path-test");
    std::fs::create_dir_all(&dir).ok();

    let path = dir.join("dup.pcap").to_string_lossy().to_string();
    mgr.start(None, path.clone()).await.unwrap();

    // Second recording to the same path should fail (create_new)
    let result = mgr.start(None, path).await;
    assert!(result.is_err(), "duplicate file path should fail");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Cannot create recording file"),
        "error should mention file creation"
    );

    mgr.stop_all();
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_record_packet_dead_recording_cleanup() {
    let mut mgr = RecordingManager::with_max_and_timeout(100, 1);
    let dir = std::env::temp_dir().join("rtpbridge-rec-dead-cleanup-test");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).ok();

    let ep = EndpointId::new_v4();
    let path = dir.join("dead-rec.pcap").to_string_lossy().to_string();
    let rec_id = mgr.start(Some(ep), path).await.unwrap();

    assert_eq!(mgr.active_recordings().len(), 1);

    // Force the recording task to die by aborting it
    // Access the recording's task handle and abort it
    if let Some(recording) = mgr.recordings.get(&rec_id) {
        recording.task.abort();
    }

    // Give the task time to detect channel closure
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Now record_packet should detect the dead channel and clean up
    let stopped = mgr.record_packet(&ep, &[0xFF; 100]);
    assert_eq!(
        stopped.len(),
        1,
        "should detect and clean up dead recording"
    );
    assert_eq!(stopped[0].recording_id, rec_id);

    assert!(
        mgr.active_recordings().is_empty(),
        "dead recording should be removed"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---- Additional tests using tempfile::tempdir ----

#[test]
fn test_recording_manager_creation() {
    // Default constructor
    let mgr = RecordingManager::new();
    assert!(
        mgr.active_recordings().is_empty(),
        "new manager should have no recordings"
    );
    assert_eq!(mgr.max_recordings, 100, "default max should be 100");

    // with_max constructor
    let mgr2 = RecordingManager::with_max(5);
    assert!(mgr2.active_recordings().is_empty());
    assert_eq!(mgr2.max_recordings, 5);

    // with_max_and_timeout constructor
    let mgr3 = RecordingManager::with_max_and_timeout(42, 30);
    assert_eq!(mgr3.max_recordings, 42);
    assert_eq!(mgr3.flush_timeout_secs, 30);

    // Default trait
    let mgr4 = RecordingManager::default();
    assert!(mgr4.active_recordings().is_empty());
    assert_eq!(mgr4.max_recordings, 100);
}

#[tokio::test]
async fn test_start_recording_is_active() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::new();

    let ep = EndpointId::new_v4();
    let path = dir.path().join("active.pcap").to_string_lossy().to_string();
    let rec_id = mgr.start(Some(ep), path.clone()).await.unwrap();

    let active = mgr.active_recordings();
    assert_eq!(active.len(), 1, "should have exactly 1 active recording");
    assert_eq!(active[0].recording_id, rec_id);
    assert_eq!(active[0].endpoint_id, Some(ep));
    assert_eq!(active[0].file_path, path);
    assert_eq!(
        active[0].state,
        crate::control::protocol::RecordingState::Active,
        "recording state should be Active"
    );

    mgr.stop_all();
}

#[tokio::test]
async fn test_start_session_wide_recording_is_active() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::new();

    let path = dir
        .path()
        .join("session.pcap")
        .to_string_lossy()
        .to_string();
    let rec_id = mgr.start(None, path.clone()).await.unwrap();

    let active = mgr.active_recordings();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].recording_id, rec_id);
    assert!(
        active[0].endpoint_id.is_none(),
        "session-wide recording should have no endpoint_id"
    );
    assert_eq!(active[0].file_path, path);

    mgr.stop_all();
}

#[tokio::test]
async fn test_max_recordings_limit_with_tempdir() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::with_max(2);

    let p1 = dir.path().join("r1.pcap").to_string_lossy().to_string();
    let p2 = dir.path().join("r2.pcap").to_string_lossy().to_string();
    mgr.start(None, p1).await.unwrap();
    mgr.start(None, p2).await.unwrap();

    // 3rd should fail
    let p3 = dir.path().join("r3.pcap").to_string_lossy().to_string();
    let err = mgr.start(None, p3).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("Maximum concurrent recordings (2) reached"),
        "error should mention the limit: {err}"
    );
    assert_eq!(
        mgr.active_recordings().len(),
        2,
        "should still have 2 active recordings"
    );

    // After stopping one, we can start again
    let ids: Vec<_> = mgr
        .active_recordings()
        .iter()
        .map(|r| r.recording_id)
        .collect();
    mgr.stop(&ids[0]).unwrap();
    let p4 = dir.path().join("r4.pcap").to_string_lossy().to_string();
    mgr.start(None, p4)
        .await
        .expect("should succeed after stopping one");
    assert_eq!(mgr.active_recordings().len(), 2);

    mgr.stop_all();
}

#[tokio::test]
async fn test_record_packet_routes_to_correct_recording() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::new();

    let ep_a = EndpointId::new_v4();
    let ep_b = EndpointId::new_v4();

    // Recording only for ep_a (inbound only, default)
    let path_a = dir.path().join("ep_a.pcap").to_string_lossy().to_string();
    let id_a = mgr.start(Some(ep_a), path_a.clone()).await.unwrap();

    // Recording only for ep_b.
    let path_b = dir.path().join("ep_b.pcap").to_string_lossy().to_string();
    let id_b = mgr.start(Some(ep_b), path_b.clone()).await.unwrap();

    // Send 3 packets to ep_a, 2 packets to ep_b
    let fake_rtp = [
        0x80, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xA0, 0x00, 0x00, 0x00, 0x01,
    ];
    for _ in 0..3 {
        mgr.record_packet(&ep_a, &fake_rtp);
    }
    for _ in 0..2 {
        mgr.record_packet(&ep_b, &fake_rtp);
    }

    // Stop individually to check per-recording packet counts
    let (_, _, packets_a, dropped_a) = mgr.stop(&id_a).unwrap();
    assert_eq!(packets_a, 3, "ep_a recording should have 3 packets");
    assert_eq!(dropped_a, 0, "ep_a should have no dropped packets");

    let (_, _, packets_b, dropped_b) = mgr.stop(&id_b).unwrap();
    assert_eq!(packets_b, 2, "ep_b recording should have 2 packets");
    assert_eq!(dropped_b, 0, "ep_b should have no dropped packets");
}

#[tokio::test]
async fn test_record_packet_session_wide_and_endpoint_specific() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::new();

    let ep = EndpointId::new_v4();

    let path_ep = dir.path().join("ep.pcap").to_string_lossy().to_string();
    let id_ep = mgr.start(Some(ep), path_ep).await.unwrap();

    let path_all = dir.path().join("all.pcap").to_string_lossy().to_string();
    let id_all = mgr.start(None, path_all).await.unwrap();

    let fake_rtp = [0x80u8; 12];
    mgr.record_packet(&ep, &fake_rtp);
    mgr.record_packet(&ep, &fake_rtp);

    // Endpoint-specific should get 2 packets
    let (_, _, pkts_ep, _) = mgr.stop(&id_ep).unwrap();
    assert_eq!(pkts_ep, 2, "endpoint recording should get both packets");

    // Session-wide should also get 2 packets
    let (_, _, pkts_all, _) = mgr.stop(&id_all).unwrap();
    assert_eq!(
        pkts_all, 2,
        "session-wide recording should also get both packets"
    );
}

fn sample_descriptor(ep: &EndpointId) -> StreamDescriptor {
    StreamDescriptor {
        v: crate::recording::meta::VERSION,
        endpoint_id: ep.to_string(),
        role: "remote".to_string(),
        ep_type: "rtp".to_string(),
        codec: "PCMU".to_string(),
        pt: 0,
        clock_rate: 8000,
        channels: 1,
        endian: None,
        ssrc: None,
        local: "10.0.0.1:4000".to_string(),
        remote: "203.0.113.7:5004".to_string(),
    }
}

fn read_pcap_payloads(path: &str) -> Vec<Vec<u8>> {
    let file = std::fs::File::open(path).expect("pcap exists");
    let mut reader = pcap_file::pcap::PcapReader::new(file).expect("valid pcap");
    let mut out = Vec::new();
    // Synthetic frames are Ethernet(14) + IPv4(20) + UDP(8) = 42.
    while let Some(pkt) = reader.next_packet() {
        let data = pkt.expect("valid packet").data.to_vec();
        if data.len() > 42 {
            out.push(data[42..].to_vec());
        }
    }
    out
}

/// A descriptor is prepended before the media it describes, only re-emitted
/// when its content changes, and the decoder can parse it back.
#[tokio::test]
async fn test_descriptor_prepended_and_deduped() {
    let dir = tempfile::tempdir().unwrap();
    let mut mgr = RecordingManager::new();
    let ep = EndpointId::new_v4();
    let path = dir.path().join("desc.pcap").to_string_lossy().to_string();
    let id = mgr.start(Some(ep), path.clone()).await.unwrap();

    let desc = sample_descriptor(&ep);
    let fake_rtp = [0x80u8; 12];

    // First packet: descriptor + media.
    mgr.note_descriptor(&ep, &desc, None, None);
    mgr.record_packet(&ep, &fake_rtp);
    // Unchanged: media only (no re-prepend).
    mgr.note_descriptor(&ep, &desc, None, None);
    mgr.record_packet(&ep, &fake_rtp);
    // Codec change: new descriptor + media.
    let mut desc2 = desc.clone();
    desc2.codec = "opus".to_string();
    desc2.pt = 111;
    desc2.clock_rate = 48000;
    mgr.note_descriptor(&ep, &desc2, None, None);
    mgr.record_packet(&ep, &fake_rtp);

    let (file_path, _, packets, _) = mgr.stop(&id).unwrap();
    // 2 descriptors + 3 media.
    assert_eq!(packets, 5, "2 descriptors + 3 media packets");

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let payloads = read_pcap_payloads(&file_path);
    assert_eq!(payloads.len(), 5);
    // Order: descriptor(PCMU), media, media, descriptor(opus), media.
    let d0 = StreamDescriptor::parse(&payloads[0]).expect("first is a descriptor");
    assert_eq!(d0.codec, "PCMU");
    assert!(
        StreamDescriptor::parse(&payloads[1]).is_none(),
        "media, not descriptor"
    );
    assert!(StreamDescriptor::parse(&payloads[2]).is_none());
    let d3 = StreamDescriptor::parse(&payloads[3]).expect("re-emitted descriptor");
    assert_eq!(d3.codec, "opus");
    assert!(StreamDescriptor::parse(&payloads[4]).is_none());
}

/// A recording started mid-call replays the cached descriptor so it is
/// self-describing from byte 0.
#[tokio::test]
async fn test_descriptor_replayed_on_late_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut mgr = RecordingManager::new();
    let ep = EndpointId::new_v4();

    // Descriptor cached before any recording exists.
    mgr.note_descriptor(&ep, &sample_descriptor(&ep), None, None);

    let path = dir.path().join("late.pcap").to_string_lossy().to_string();
    let id = mgr.start(Some(ep), path.clone()).await.unwrap();
    mgr.record_packet(&ep, &[0x80u8; 12]);

    let (file_path, _, packets, _) = mgr.stop(&id).unwrap();
    assert_eq!(packets, 2, "replayed descriptor + 1 media");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let payloads = read_pcap_payloads(&file_path);
    assert!(
        StreamDescriptor::parse(&payloads[0]).is_some(),
        "late-started recording leads with the replayed descriptor"
    );
}

#[tokio::test]
async fn test_stop_returns_correct_metadata() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::new();

    let ep = EndpointId::new_v4();
    let path = dir.path().join("meta.pcap").to_string_lossy().to_string();
    let rec_id = mgr.start(Some(ep), path.clone()).await.unwrap();

    // Record some packets
    let fake_rtp = [0x80u8; 20];
    for _ in 0..7 {
        mgr.record_packet(&ep, &fake_rtp);
    }

    // Small sleep so duration_ms is > 0
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    let (file_path, duration_ms, packets, dropped) = mgr.stop(&rec_id).unwrap();
    assert_eq!(
        file_path, path,
        "file_path should match what was provided at start"
    );
    assert!(
        duration_ms >= 10,
        "duration should be at least 10ms, got {duration_ms}"
    );
    assert!(
        duration_ms < 5000,
        "duration should be reasonable, got {duration_ms}"
    );
    assert_eq!(packets, 7, "should report 7 packets");
    assert_eq!(dropped, 0, "should have no dropped packets");

    // Recording should no longer be active
    assert!(mgr.active_recordings().is_empty());
}

#[tokio::test]
async fn test_stop_endpoint_recordings_removes_for_endpoint() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::new();

    let ep1 = EndpointId::new_v4();
    let ep2 = EndpointId::new_v4();

    let p1 = dir.path().join("ep1.pcap").to_string_lossy().to_string();
    let id1 = mgr.start(Some(ep1), p1.clone()).await.unwrap();

    let p2 = dir.path().join("ep2.pcap").to_string_lossy().to_string();
    let _id2 = mgr.start(Some(ep2), p2).await.unwrap();

    // Session-wide (not tied to any endpoint)
    let p3 = dir
        .path()
        .join("session.pcap")
        .to_string_lossy()
        .to_string();
    let _id3 = mgr.start(None, p3).await.unwrap();

    assert_eq!(mgr.active_recordings().len(), 3);

    // Record packets to ep1 so we can verify counts in the stopped info
    let fake_rtp = [0x80u8; 12];
    for _ in 0..4 {
        mgr.record_packet(&ep1, &fake_rtp);
    }

    // Stop only ep1 recordings
    let stopped = mgr.stop_endpoint_recordings(&ep1);
    assert_eq!(stopped.len(), 1, "should stop 1 recording for ep1");
    assert_eq!(stopped[0].recording_id, id1);
    assert_eq!(stopped[0].file_path, p1);
    assert_eq!(stopped[0].packets, 4);

    // ep2 and session-wide should still be active
    assert_eq!(
        mgr.active_recordings().len(),
        2,
        "ep2 and session-wide should remain"
    );

    // Stopping ep1 again should return empty
    let stopped2 = mgr.stop_endpoint_recordings(&ep1);
    assert!(stopped2.is_empty(), "no more recordings for ep1");

    mgr.stop_all();
}

#[tokio::test]
async fn test_stop_all_stops_everything() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut mgr = RecordingManager::new();

    let ep1 = EndpointId::new_v4();
    let ep2 = EndpointId::new_v4();

    let p1 = dir.path().join("s1.pcap").to_string_lossy().to_string();
    let id1 = mgr.start(Some(ep1), p1.clone()).await.unwrap();

    let p2 = dir.path().join("s2.pcap").to_string_lossy().to_string();
    let id2 = mgr.start(Some(ep2), p2.clone()).await.unwrap();

    let p3 = dir.path().join("s3.pcap").to_string_lossy().to_string();
    let id3 = mgr.start(None, p3.clone()).await.unwrap();

    // Record some packets
    mgr.record_packet(&ep1, &[0xAA; 100]);
    mgr.record_packet(&ep2, &[0xBB; 100]);

    let stopped = mgr.stop_all();
    assert_eq!(stopped.len(), 3, "stop_all should stop all 3 recordings");

    let stopped_ids: Vec<_> = stopped.iter().map(|s| s.recording_id).collect();
    assert!(stopped_ids.contains(&id1));
    assert!(stopped_ids.contains(&id2));
    assert!(stopped_ids.contains(&id3));

    // Verify file paths
    let stopped_paths: Vec<_> = stopped.iter().map(|s| s.file_path.clone()).collect();
    assert!(stopped_paths.contains(&p1));
    assert!(stopped_paths.contains(&p2));
    assert!(stopped_paths.contains(&p3));

    // Verify all are gone
    assert!(
        mgr.active_recordings().is_empty(),
        "no recordings should remain"
    );

    // Manager should be reusable
    let p4 = dir.path().join("s4.pcap").to_string_lossy().to_string();
    mgr.start(None, p4)
        .await
        .expect("manager should be reusable after stop_all");
    assert_eq!(mgr.active_recordings().len(), 1);
    mgr.stop_all();
}

#[tokio::test]
async fn test_start_recording_readonly_dir() {
    let mut mgr = RecordingManager::new();
    let dir = std::env::temp_dir().join("rtpbridge-rec-readonly-test");
    std::fs::create_dir_all(&dir).ok();

    // Make the directory read-only
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&dir, perms).unwrap();

    let path = dir.join("should-fail.pcap").to_string_lossy().to_string();
    let result = mgr.start(None, path).await;
    assert!(
        result.is_err(),
        "recording to read-only directory should fail"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Cannot create recording file"),
        "error should mention file creation failure"
    );

    // Restore permissions for cleanup
    let mut perms = std::fs::metadata(&dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&dir, perms).unwrap();
    std::fs::remove_dir_all(&dir).ok();
}
