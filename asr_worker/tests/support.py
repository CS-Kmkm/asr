"""Isolation shared by unittest discovery and pytest runs."""
from __future__ import annotations

import os
import unittest
from unittest.mock import patch


class IsolatedWorkerTestCase(unittest.TestCase):
    def setUp(self) -> None:
        # Developer worker settings must not select another model, provider,
        # delay or endpoint while unit tests construct their fake backend.
        environment = {key: value for key, value in os.environ.items() if not key.upper().startswith("ASR_")}
        isolation = patch.dict(os.environ, environment, clear=True)
        isolation.start()
        self.addCleanup(isolation.stop)
