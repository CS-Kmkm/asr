import unittest
from unittest.mock import Mock

from asr_worker.backends import BackendError
from asr_worker.decision import DecisionModel, RELEASES


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
        request = {"model": model.identity[0], "state": {"spoken": "短くして", "has_selection": True}, "questions": {"mode": {"type": "choice"}}}
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

