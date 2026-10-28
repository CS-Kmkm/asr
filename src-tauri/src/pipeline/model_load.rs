//! Safe, actionable copy for errors crossing the model-loading IPC boundary.
use crate::asr::AsrError;

pub(crate) fn model_load_failure_message(error: &AsrError) -> &'static str {
    match error {
        AsrError::Cancelled => "Speech model loading was cancelled.",
        AsrError::Timeout => "Speech model loading timed out. Check the network and model files, then try again.",
        AsrError::Crashed => "The speech worker stopped while loading the model. Restart the app and try a smaller model.",
        AsrError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => "The speech worker could not be found. Run worker setup, then try loading again.",
        AsrError::Io(_) | AsrError::Protocol(_) => "The speech worker could not communicate correctly. Restart the app, then try loading again.",
        AsrError::Worker(error) => match error.code.as_str() {
            "worker_source_missing" => "Speech worker files are missing. Reinstall the app, or run uv sync from the development checkout.",
            "worker_setup_unavailable" => "Worker setup requires uv. Install uv or set ASR_UV to its executable, then load the model again.",
            "worker_setup_failed" => "Worker setup failed. Check the network, free disk space and write permission, then load the model again.",
            "worker_version_mismatch" | "unsupported_operation" => "The speech worker and app versions do not match. Update the app and run worker setup again.",
            "gpu_oom" | "cuda_oom" => "There is not enough GPU memory to load this model. Choose a smaller model or use faster-whisper on CPU.",
            "cpu_oom" => "There is not enough system memory to load this model. Close other apps or choose a smaller model.",
            "gpu_unsupported" => "This model requires a supported CUDA GPU. Choose faster-whisper to use the CPU.",
            "backend_unavailable" => "The speech worker is missing required packages. Run worker setup, then try loading again.",
            "unsupported_quantization" => "This model does not support the selected quantization. Choose another quantization in Models.",
            "hf_auth_required" => "Model download requires Hugging Face access. Check HF_TOKEN and the model access permission, then restart the app.",
            "hf_rate_limited" => "Hugging Face is limiting model downloads. Wait and try again; set HF_TOKEN for higher limits.",
            "hf_repository_unavailable" => "The model repository is unavailable. Check the model ID and Hugging Face access permission.",
            "hf_download_failed" => "The model download failed. Check the network connection and try again.",
            "invalid_api_config" => "The speech provider configuration is invalid. Check its endpoint and API key in Models.",
            _ => "The speech model could not be loaded. Check worker setup and the model configuration, then try again.",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::WorkerError;

    #[test]
    fn cpu_load_exhaustion_does_not_suggest_gpu_remediation() {
        let error = AsrError::Worker(WorkerError {
            code: "cpu_oom".into(),
            message: "private worker details".into(),
        });
        let message = model_load_failure_message(&error);
        assert!(message.contains("system memory"));
        assert!(!message.contains("GPU"));
        assert!(!message.contains("private"));
        assert!(crate::messages::code(message).is_some());
    }

    #[test]
    fn model_load_failures_are_registered_and_do_not_echo_worker_or_os_details() {
        let mut errors = vec![
            AsrError::Timeout,
            AsrError::Crashed,
            AsrError::Cancelled,
            AsrError::Protocol("private-endpoint-and-secret".into()),
            AsrError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "private-endpoint-and-secret",
            )),
            AsrError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private-endpoint-and-secret",
            )),
        ];
        errors.extend(
            [
                "gpu_oom",
                "cpu_oom",
                "cuda_oom",
                "gpu_unsupported",
                "backend_unavailable",
                "unsupported_quantization",
                "hf_auth_required",
                "hf_rate_limited",
                "hf_repository_unavailable",
                "hf_download_failed",
                "invalid_api_config",
                "worker_version_mismatch",
                "worker_source_missing",
                "worker_setup_unavailable",
                "worker_setup_failed",
                "unsupported_operation",
                "unknown",
            ]
            .map(|code| {
                AsrError::Worker(WorkerError {
                    code: code.into(),
                    message: "private-endpoint-and-secret".into(),
                })
            }),
        );
        for error in errors {
            let message = model_load_failure_message(&error);
            assert!(crate::messages::code(message).is_some(), "{message}");
            assert!(!message.contains("private-endpoint-and-secret"));
        }
        assert_ne!(
            model_load_failure_message(&AsrError::Timeout),
            model_load_failure_message(&AsrError::Crashed)
        );
    }
}
