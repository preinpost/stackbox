"""Keep Ansible result payloads (including service passwords) out of portal logs."""

import json
import os

from ansible.plugins.callback import CallbackBase


class CallbackModule(CallbackBase):
    CALLBACK_VERSION = 2.0
    CALLBACK_TYPE = "stdout"
    CALLBACK_NAME = "farm"

    def v2_playbook_on_task_start(self, task, is_conditional):
        self._display.display("TASK " + task.get_name().strip())

    def v2_playbook_on_handler_task_start(self, task):
        self._display.display("HANDLER " + task.get_name().strip())

    def v2_runner_on_ok(self, result):
        status = "changed" if result._result.get("changed") else "ok"
        self._display.display(status + ": " + result._host.get_name())

    def v2_runner_on_failed(self, result, ignore_errors=False):
        self._failure(result, "FAILED", ignore_errors)

    def v2_runner_on_unreachable(self, result):
        self._failure(result, "UNREACHABLE", False)

    def _failure(self, result, status, ignored):
        host = result._host.get_name()
        path = "/var/log/farm/ansible-failures.jsonl"
        # Retain diagnostic details inside the all-in-one VM, accessible only to root.
        # Respect upstream no_log even in this private file.
        payload = ("censored by Ansible no_log" if result._result.get("_ansible_no_log")
                   else result._result)
        record = {"host": host, "task": result._task.get_name(), "result": payload}
        fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
        with os.fdopen(fd, "a") as output:
            output.write(json.dumps(record, default=str) + "\n")
        self._display.display(status + (" (ignored)" if ignored else "") + ": " + host
                              + " (details on all-in-one VM: " + path + ")")

    def v2_playbook_on_stats(self, stats):
        self._display.display("PLAY RECAP")
        for host in sorted(stats.processed):
            summary = stats.summarize(host)
            self._display.display(host + ": " + " ".join(
                str(key) + "=" + str(value) for key, value in summary.items()))
