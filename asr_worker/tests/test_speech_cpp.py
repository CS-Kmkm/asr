from __future__ import annotations

import hashlib
import io
import os
from pathlib import Path
import tempfile
import subprocess
import time
import zipfile
from unittest.mock import MagicMock, patch

import httpx

from asr_worker.backends import BackendError, create_backend
from asr_worker.speech_cpp import SpeechCppBackend, acquire, verify, publish_bytes, clean_staging, server_environment, prepare, ARCHIVE
from asr_worker.tests.support import IsolatedWorkerTestCase


class SpeechCppTests(IsolatedWorkerTestCase):
    def test_atomic_extraction_rejects_bad_hash_and_interrupted_publication(self):
        with tempfile.TemporaryDirectory() as folder:
            target = Path(folder) / 'speech.dll'
            with self.assertRaises(BackendError):
                publish_bytes(b'bad bytes', target, '0' * 64)
            self.assertFalse(target.exists())
            with patch('asr_worker.speech_cpp.os.link', side_effect=SystemExit('interrupted')):
                with self.assertRaises(SystemExit):
                    publish_bytes(b'valid', target, hashlib.sha256(b'valid').hexdigest())
            self.assertEqual(list(Path(folder).iterdir()), [])

    def test_stale_cleanup_removes_only_owned_old_partials_of_dead_processes(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            names = ['.download-111-abcdefgh', '.download-222-abcdefgh', '.download-333-abcdefgh', '.download-legacy', 'model.gguf']
            for name in names:
                path = root / name
                path.write_bytes(b'preserve unless old dead partial')
                os.utime(path, (time.time() - 7200, time.time() - 7200))
            os.utime(root / names[2], None)
            with patch('asr_worker.speech_cpp.process_alive', side_effect=lambda pid: pid == 222):
                clean_staging(root)
            self.assertEqual({p.name for p in root.iterdir()}, set(names[1:]))

    def test_server_environment_keeps_windows_runtime_but_drops_credentials(self):
        with patch.dict(os.environ, {'SYSTEMROOT': 'C:/Windows', 'PATH': 'runtime-path', 'HF_TOKEN': 'secret', 'HTTPS_PROXY': 'private'}, clear=True):
            self.assertEqual(server_environment(), {'SYSTEMROOT': 'C:/Windows', 'PATH': 'runtime-path'})

    def test_prepare_pinned_zip_good_bad_missing_and_partial_files(self):
        for variant in ['good', 'bad-hash', 'missing', 'preexisting-partial']:
            with self.subTest(variant=variant), tempfile.TemporaryDirectory() as folder:
                home = Path(folder)
                expected = hashlib.sha256(b'exe').hexdigest()
                with zipfile.ZipFile(home / ARCHIVE, 'w') as bundle:
                    if variant != 'missing':
                        bundle.writestr('speech.exe', b'corrupt' if variant == 'bad-hash' else b'exe')
                if variant == 'preexisting-partial':
                    (home / 'runtime-0.10.1').mkdir()
                    (home / 'runtime-0.10.1/speech.exe').write_bytes(b'partial')
                (home / 'revision').mkdir()
                (home / 'revision/model.gguf').write_bytes(b'gguf')
                model = {'file': 'model.gguf', 'revision': 'revision', 'repo': 'example/model', 'sha256': hashlib.sha256(b'gguf').hexdigest()}
                backend = SpeechCppBackend(); backend.catalog_id = 'test'
                with patch('asr_worker.speech_cpp.IS_WINDOWS', True), \
                     patch('asr_worker.speech_cpp.platform.machine', return_value='AMD64'), \
                     patch('asr_worker.speech_cpp.cache_home', return_value=home), \
                     patch('asr_worker.speech_cpp.RUNTIME_FILES', {'speech.exe': expected}), \
                     patch('asr_worker.speech_cpp.MODELS', {'test': model}), \
                     patch('asr_worker.speech_cpp.ARCHIVE_SHA256', hashlib.sha256((home / ARCHIVE).read_bytes()).hexdigest()), \
                     patch('asr_worker.speech_cpp.acquire', side_effect=lambda url, path, sha, backend: verify(path, sha)):
                    if variant == 'good':
                        exe, gguf = prepare(backend)
                        self.assertEqual(exe.read_bytes(), b'exe')
                        self.assertEqual(gguf.read_bytes(), b'gguf')
                    else:
                        with self.assertRaises((BackendError, KeyError)):
                            prepare(backend)
                        if variant == 'preexisting-partial':
                            self.assertEqual((home / 'runtime-0.10.1/speech.exe').read_bytes(), b'partial')
                        else:
                            self.assertFalse((home / 'runtime-0.10.1/speech.exe').exists())
        with patch('asr_worker.speech_cpp.IS_WINDOWS', False):
            with self.assertRaises(BackendError):
                prepare(SpeechCppBackend())
        with patch('asr_worker.speech_cpp.IS_WINDOWS', True), patch('asr_worker.speech_cpp.platform.machine', return_value='ARM64'), patch('asr_worker.speech_cpp.acquire') as network:
            with self.assertRaises(BackendError) as caught:
                prepare(SpeechCppBackend())
            self.assertEqual(caught.exception.code, 'speech_cpp_unsupported_platform')
            network.assert_not_called()

    def test_unload_stuck_child_resets_state_and_closes_job(self):
        backend = SpeechCppBackend()
        proc = MagicMock(); proc.poll.return_value = None
        proc.wait.side_effect = subprocess.TimeoutExpired('speech', 5)
        kernel = MagicMock()
        backend.process = proc; backend.job = (kernel, 'handle')
        backend.device = 'vulkan'
        with self.assertRaises(BackendError) as caught:
            backend.unload()
        self.assertEqual(caught.exception.code, 'speech_cpp_unavailable')
        self.assertIsNone(backend.process)
        self.assertIsNone(backend.job)
        self.assertIsNone(backend.device)
        kernel.CloseHandle.assert_called_once_with('handle')
        backend.unload()

    def test_artifact_mismatch_preserves_existing_file_and_never_downloads(self):
        with tempfile.TemporaryDirectory() as folder:
            target = Path(folder) / 'model.gguf'
            target.write_bytes(b'original')
            with patch('httpx.stream') as network:
                with self.assertRaises(BackendError) as caught:
                    acquire('https://example.invalid', target, '0' * 64, SpeechCppBackend())
            self.assertEqual(caught.exception.code, 'speech_cpp_artifact_invalid')
            self.assertEqual(target.read_bytes(), b'original')
            network.assert_not_called()

    def test_download_verifies_before_publication_and_cancel_cleans_staging(self):
        content = b'GGUF-test'
        expected = hashlib.sha256(content).hexdigest()
        response = MagicMock()
        response.headers = {'content-length': str(len(content))}
        response.iter_bytes.return_value = iter([content])
        with tempfile.TemporaryDirectory() as folder, patch('httpx.stream') as network:
            network.return_value.__enter__.return_value = response
            target = Path(folder) / 'model.gguf'
            backend = SpeechCppBackend()
            acquire('https://example.invalid', target, expected, backend)
            verify(target, expected)
            response.iter_bytes.return_value = iter([content])
            backend.cancel_requested = lambda: True
            with self.assertRaises(BackendError) as caught:
                acquire('https://example.invalid', Path(folder) / 'cancelled.gguf', expected, backend)
            self.assertEqual(caught.exception.code, 'cancelled')
            self.assertEqual([p.name for p in Path(folder).iterdir()], ['model.gguf'])
            backend.cancel_requested = None
            response.iter_bytes.return_value = iter([content])
            with self.assertRaises(BackendError):
                acquire('https://example.invalid', Path(folder) / 'bad.gguf', '0' * 64, backend)
            self.assertEqual([p.name for p in Path(folder).iterdir()], ['model.gguf'])

    def _start(self, *, ready=True, models=None, job_error=False):
        backend = SpeechCppBackend()
        process = MagicMock()
        process.poll.return_value = None
        process.stdout = io.BytesIO(b'Ready at http://127.0.0.1:12345/\n' if ready else b'')
        process.terminate.side_effect = lambda: setattr(process.poll, 'return_value', 0)
        client = MagicMock()
        client.get.return_value.json.return_value = {'data': models if models is not None else [
            {'id': 'Haas-ja-v1.0', 'speech': {'task': 'recognition', 'device': 'Vulkan0', 'weight_type': 'Q8_0'}}]}
        with patch('asr_worker.speech_cpp.prepare', return_value=(Path('speech.exe'), Path('test.gguf'))), \
             patch('asr_worker.speech_cpp.subprocess.run', return_value=MagicMock(stdout=b'Vulkan0    gpu   NVIDIA GeForce RTX 3060, 12.9 GB\n')), \
             patch('asr_worker.speech_cpp.subprocess.Popen', return_value=process) as start, \
             patch('asr_worker.speech_cpp.attach_job', side_effect=OSError('job denied') if job_error else None, return_value=None), \
             patch('httpx.Client', return_value=client) as transport:
            if ready and models != [] and not job_error:
                backend.load('4bit')
            else:
                with self.assertRaises(BackendError) as caught:
                    backend.load('8bit')
                self.assertEqual(caught.exception.code, 'speech_cpp_unavailable')
                self.assertIsNone(backend.process)
        return backend, process, client, start, transport

    def test_managed_loopback_lifecycle_ignores_cloud_configuration(self):
        with patch.dict(os.environ, {'OPENAI_API_KEY': 'secret', 'HF_TOKEN': 'private-hf', 'ASR_API_BASE_URL': 'https://example.invalid/v1'}):
            backend, proc, client, start, transport = self._start()
        self.assertEqual(backend.base_url, 'http://127.0.0.1:12345/v1')
        self.assertIsNone(backend.api_key)
        self.assertFalse(transport.call_args.kwargs['trust_env'])
        self.assertNotIn('headers', transport.call_args.kwargs)
        self.assertEqual((backend.device, backend.compute_type), ('vulkan', 'Q8_0'))
        self.assertIn('Vulkan0', start.call_args.args[0])
        self.assertNotIn('OPENAI_API_KEY', start.call_args.kwargs['env'])
        self.assertNotIn('HF_TOKEN', start.call_args.kwargs['env'])
        self.assertNotIn('ASR_API_BASE_URL', start.call_args.kwargs['env'])
        backend.unload()
        backend.unload()
        proc.terminate.assert_called_once()
        client.close.assert_called_once()
        self.assertIsNone(backend.device)

    def test_startup_failure_job_failure_and_bad_model_list_stop_child(self):
        for options in [{'ready': False}, {'job_error': True}, {'models': []}]:
            with self.subTest(options=options):
                _, proc, _, _, _ = self._start(**options)
                proc.terminate.assert_called_once()

    def test_non_japanese_rejected_before_audio_is_opened_or_sent(self):
        backend = create_backend('speech-cpp')
        with self.assertRaises(BackendError) as caught:
            backend.transcribe(Path('absent.wav'), None, 'en-US')
        self.assertEqual(caught.exception.code, 'speech_cpp_language')

    def test_free_text_prompt_is_not_sent_to_fastconformer(self):
        backend = SpeechCppBackend()
        backend.process = MagicMock()
        backend.process.poll.return_value = None
        captured = []
        def handle(request):
            captured.append(request.read())
            return httpx.Response(200, json={'text': '日本語'})
        backend.client = httpx.Client(base_url='http://127.0.0.1/v1/', transport=httpx.MockTransport(handle))
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'audio.wav'
            path.write_bytes(b'RIFF')
            text, _ = backend.transcribe(path, 'private reading => private term', 'ja-JP')
        backend.client.close()
        self.assertEqual(text, '日本語')
        self.assertNotIn(b'name="prompt"', captured[0])
        self.assertIn(b'\r\n\r\nja\r\n', captured[0])
        backend.process.poll.return_value = 1
        with self.assertRaises(BackendError) as caught:
            backend.transcribe(Path('absent.wav'), None, 'ja')
        self.assertEqual(caught.exception.code, 'speech_cpp_unavailable')
