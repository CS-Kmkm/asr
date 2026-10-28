import unittest
import sys
import weakref
from unittest.mock import Mock, patch

from asr_worker.backends import BackendError
from asr_worker.decision import DecisionModel, RELEASES
from asr_worker.worker import Worker
from asr_worker.backends import MockBackend


class DecisionTests(unittest.TestCase):
    def test_only_pinned_official_releases_can_execute_model_code(self):
        self.assertEqual(set(RELEASES), {"Cloudflare/clef", "Cloudflare/clef-flash"})
        self.assertTrue(all(len(revision) == 40 for revision in RELEASES.values()))
        with self.assertRaises(BackendError):
            DecisionModel().load("untrusted/repo", "4bit")

    def test_local_classification_forwards_typed_input_without_source_text(self):
        model = DecisionModel()
        model.identity = ("Cloudflare/clef-flash", "4bit")
        model.systemone = Mock(return_value={"answers": {"mode": {"choice": "edit"}}})
        request = {"model": model.identity[0], "quantization": "4bit", "state": {"spoken": "短くして", "has_selection": True}, "questions": {"mode": {"type": "choice"}}}
        self.assertEqual(model.classify(request)["answers"]["mode"]["choice"], "edit")
        model.systemone.assert_called_once_with(None, None, request, max_length=12288)
        for state in [{"spoken": "", "has_selection": True}, {"spoken": "x", "has_selection": "yes"}, {"spoken": "x", "has_selection": True, "selected_source": "private"}]:
            with self.assertRaises(BackendError):
                model.classify({**request, "state": state})
        self.assertEqual(model.systemone.call_count, 1)

    def test_loaded_identity_reuses_weights_and_quantization(self):
        model = DecisionModel()
        model.identity = ("Cloudflare/clef-flash", "4bit")
        model.model = object()
        original = model.model
        model.load("Cloudflare/clef-flash", "4bit")
        self.assertIs(model.model, original)

    def test_wrong_quantization_does_not_use_a_stale_loaded_model(self):
        model = DecisionModel()
        model.identity = ("Cloudflare/clef-flash", "4bit")
        model.systemone = Mock()
        with self.assertRaises(BackendError):
            model.classify({"model": model.identity[0], "quantization": "8bit",
                            "state": {"spoken": "hello", "has_selection": False}})
        model.systemone.assert_not_called()

    def test_worker_cancellation_discards_decision_and_never_starts_a_cancelled_load(self):
        worker = Worker(MockBackend())
        with patch.object(worker.decision, "load") as load:
            response, _ = worker.handle({"id": 1, "command": "load_decision", "model": "Cloudflare/clef-flash"}, lambda: True)
            self.assertEqual(response["error"]["code"], "cancelled")
            load.assert_not_called()
        checks = iter([False, True])
        with patch.object(worker.decision, "classify", return_value={"answers": {}}):
            response, _ = worker.handle({"id": 2, "command": "classify_decision", "input": {}}, lambda: next(checks))
        self.assertEqual(response["error"]["code"], "cancelled")

    def test_worker_shutdown_releases_both_models(self):
        worker = Worker(MockBackend())
        with patch.object(worker.decision, "unload") as unload:
            response, stop = worker.handle({"id": 1, "command": "shutdown"})
        self.assertTrue(response["ok"])
        self.assertTrue(stop)
        unload.assert_called_once_with()

    def test_failed_release_load_clears_partial_models_and_cuda_cache(self):
        torch = Mock()
        torch.cuda.is_available.return_value = True
        modules = {name: Mock() for name in ["httpx", "truststore", "huggingface_hub", "transformers"]}
        modules["torch"] = torch
        model = DecisionModel()
        references = []
        class PartialModel:
            pass
        def fail(*args):
            def inner_loader():
                partial = PartialModel()
                references.append(weakref.ref(partial))
                raise ValueError("synthetic inner load failure")
            try:
                inner_loader()
            except ValueError as error:
                raise RuntimeError("synthetic load failure") from error
        def check_released():
            if references:
                self.assertIsNone(references[0]())
        torch.cuda.empty_cache.side_effect = check_released
        with patch.dict(sys.modules, modules), patch.object(model, "_load_release", side_effect=fail):
            with self.assertRaises(RuntimeError):
                model.load("Cloudflare/clef-flash", "4bit")
        self.assertIsNone(model.model)
        self.assertIsNone(model.identity)
        self.assertGreaterEqual(torch.cuda.empty_cache.call_count, 2)

    def test_unload_decision_keeps_the_speech_model_loaded(self):
        worker = Worker(MockBackend())
        worker.handle({"id": 1, "command": "load", "quantization": "bf16"})
        worker.decision.identity = ("Cloudflare/clef-flash", "4bit")
        response, stop = worker.handle({"id": 2, "command": "unload_decision"})
        self.assertTrue(response["ok"])
        self.assertFalse(stop)
        self.assertTrue(worker.loaded)
        self.assertIsNone(worker.decision.identity)

    def test_cancel_after_successful_load_releases_decision_and_keeps_speech(self):
        worker = Worker(MockBackend())
        worker.loaded = True
        def load(*args):
            worker.decision.identity = ("Cloudflare/clef-flash", "4bit")
        checks = iter([False, True])
        with patch.object(worker.decision, "load", side_effect=load):
            response, _ = worker.handle({"id": 1, "command": "load_decision", "model": "Cloudflare/clef-flash"}, lambda: next(checks))
        self.assertEqual(response["error"]["code"], "cancelled")
        self.assertIsNone(worker.decision.identity)
        self.assertTrue(worker.loaded)
