import importlib.util
import sys
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("resource_metrics_tests", Path(__file__).resolve().parents[2] / "scripts/resource_metrics.py")
resources = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = resources
spec.loader.exec_module(resources)


class Process:
    def __init__(self, pid, cpu, rss, children=None):
        self.pid, self.cpu, self.rss, self.descendants = pid, cpu, rss, children or []
    def create_time(self): return self.pid * 10
    def cpu_times(self): return SimpleNamespace(user=self.cpu, system=0)
    def memory_info(self): return SimpleNamespace(rss=self.rss)
    def children(self, recursive): return self.descendants


def test_resource_samples_sum_observed_children_and_leave_unavailable_gpu_missing():
    child = Process(12, 1, 40)
    root = Process(11, 5, 100, [child])
    module = SimpleNamespace(Process=lambda pid: root, Error=OSError)
    monitor = resources.ResourceMonitor(11, process_module=module)
    monitor.root, monitor.root_identity = root, root.create_time()
    monitor.gpu_tool = None
    with patch.object(resources.time, "perf_counter", side_effect=[10, 11]):
        monitor.sample()
        root.cpu += 0.25; child.cpu += 0.5; root.rss += 20
        monitor.sample()
    report = monitor.report()
    assert report["rss_sum_sampled_peak_bytes"] == 160
    assert report["cpu_observed_seconds"] == 0.75
    assert report["cpu_peak_percent_one_core_100"] == 75
    assert report["gpu_memory_sampled_peak_bytes"] is None


def test_resource_sampling_is_optional_and_missing_psutil_does_not_fail_a_run():
    with resources.ResourceMonitor(None, enabled=False) as monitor:
        assert monitor.report()["status"] == "disabled"
    with patch.object(resources.importlib, "import_module", side_effect=ImportError):
        with resources.ResourceMonitor(11) as monitor:
            report = monitor.report()
    assert report["status"] == "unavailable"
    assert report["rss_sum_sampled_peak_bytes"] is None


def test_late_observed_child_does_not_charge_preexisting_lifetime_cpu():
    root = Process(11, 5, 100)
    monitor = resources.ResourceMonitor(11, process_module=SimpleNamespace(Error=OSError))
    monitor.root, monitor.root_identity, monitor.gpu_tool = root, root.create_time(), None
    with patch.object(resources.time, "perf_counter", side_effect=[10, 11, 12]):
        monitor.sample()
        child = Process(12, 50, 20); root.descendants = [child]
        monitor.sample()
        child.cpu += 0.25
        monitor.sample()
    assert monitor.report()["cpu_observed_seconds"] == 0.25
    assert monitor.report()["cpu_peak_percent_one_core_100"] == 25


def test_nvidia_values_are_for_selected_pids_and_na_is_missing():
    monitor = resources.ResourceMonitor(11)
    monitor.gpu_tool = "nvidia-smi"
    with patch.object(resources.subprocess, "run", return_value=SimpleNamespace(stdout="11, 100\n99, 900\n12, 50\n")):
        assert monitor._gpu_memory({11, 12}) == 150 * 1024 * 1024
    with patch.object(resources.subprocess, "run", return_value=SimpleNamespace(stdout="11, [N/A]\n")):
        assert monitor._gpu_memory({11}) is None
