//! Opt-in physical USB microphone smoke for the ASR-Eval guest.

use super::*;
use std::{process::Command, time::Instant};

const POLL: Duration = Duration::from_millis(200);
const OPERATOR_TIMEOUT: Duration = Duration::from_secs(60);

async fn wait_for_speech(capture: &CpalAudioCapture, vad: EnergyVadConfig) {
    let deadline = Instant::now() + OPERATOR_TIMEOUT;
    while Instant::now() < deadline {
        let snapshot = capture.snapshot(Duration::ZERO).unwrap();
        if EnergyVad::new(vad)
            .analyze(&snapshot.samples, snapshot.input_sample_rate)
            .voiced_duration
            >= Duration::from_millis(300)
        {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("no speech detected on the selected microphone");
}

fn endpoint_present() -> bool {
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let result = Command::new(powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; $device = Get-PnpDevice -PresentOnly -Class AudioEndpoint | Where-Object InstanceId -eq $env:ASR_VM_MIC_INSTANCE_ID; if ($device) { 'present' } else { 'absent' }",
        ])
        .output()
        .expect("query guest PnP endpoints");
    assert!(result.status.success(), "guest PnP query failed");
    match String::from_utf8_lossy(&result.stdout).trim() {
        "present" => true,
        "absent" => false,
        _ => panic!("unexpected guest PnP status"),
    }
}

#[tokio::test]
#[ignore = "requires a physical USB microphone redirected to ASR-Eval and an operator"]
async fn physical_usb_microphone_disconnect_and_reconnect() {
    assert_eq!(std::env::var("COMPUTERNAME").as_deref(), Ok("ASR-EVAL"));
    let name = std::env::var("ASR_VM_MIC_NAME").expect("run check_vm_microphone_hotplug.ps1");
    assert!(!name.is_empty());
    assert!(std::env::var_os("ASR_VM_MIC_INSTANCE_ID").is_some());
    assert!(endpoint_present(), "selected guest endpoint is absent");
    let scratch = tempfile::tempdir().unwrap();
    let config = CaptureConfig {
        device_id: Some(format!("name:{name}")),
        artifact_directory: Some(scratch.path().to_path_buf()),
        ..CaptureConfig::default()
    };
    let mut capture = CpalAudioCapture::default();
    capture.start(config.clone()).await.unwrap();
    eprintln!("Speak into the selected USB microphone.");
    wait_for_speech(&capture, config.vad).await;

    eprintln!("Unplug the USB microphone. Leave it unplugged until prompted.");
    let deadline = Instant::now() + OPERATOR_TIMEOUT;
    loop {
        if capture.stream_error().is_some() && !capture.recover_stream() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "capture did not observe a disconnect"
        );
        tokio::time::sleep(POLL).await;
    }
    assert!(
        !endpoint_present(),
        "the original PnP endpoint is still present"
    );
    let before = capture
        .stop()
        .await
        .expect("preserve audio before disconnect");
    assert!(before.warning.is_some() && before.vad.contains_voice);
    eprintln!(
        "Preserved {} ms before disconnect. Reconnect the microphone.",
        before.duration.as_millis()
    );

    let deadline = Instant::now() + OPERATOR_TIMEOUT;
    loop {
        if endpoint_present()
            && capture
                .list_devices()
                .await
                .unwrap()
                .iter()
                .any(|device| device.name == name)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the same guest endpoint did not return"
        );
        tokio::time::sleep(POLL).await;
    }
    capture.start(config.clone()).await.unwrap();
    eprintln!("Speak again into the same USB microphone.");
    wait_for_speech(&capture, config.vad).await;
    let after = capture.stop().await.unwrap();
    assert!(after.warning.is_none() && after.vad.contains_voice);
    eprintln!(
        "Recorded {} ms after reconnect.",
        after.duration.as_millis()
    );
}
