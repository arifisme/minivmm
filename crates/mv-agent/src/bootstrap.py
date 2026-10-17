import importlib
import json
import os
import random
import sys
import time
import traceback


class Context:
    def __init__(self, request):
        self.request_id = request["request_id"]
        self.aws_request_id = self.request_id
        self.deadline_ms = request["deadline_ms"]
        self.function_name = os.environ.get("MV_FUNCTION_NAME", "")
        self.memory_limit_in_mb = int(os.environ.get("MV_FUNCTION_MEMORY_MB", "0"))

    def get_remaining_time_in_millis(self):
        return max(0, int(self.deadline_ms - time.time() * 1000))


def describe(exc):
    return {
        "type": type(exc).__name__,
        "message": str(exc),
        "trace": traceback.format_exception(type(exc), exc, exc.__traceback__),
    }


def main():
    module_name, function_name = sys.argv[1], sys.argv[2]
    protocol = os.fdopen(os.dup(1), "w")
    os.dup2(2, 1)
    sys.stdout = sys.stderr

    def send(message):
        protocol.write(json.dumps(message, allow_nan=False) + "\n")
        protocol.flush()

    try:
        sys.path.insert(0, os.getcwd())
        handler = getattr(importlib.import_module(module_name), function_name)
        if not callable(handler):
            raise TypeError(f"{module_name}.{function_name} is not callable")
    except BaseException as exc:
        send({"error": describe(exc)})
        return 1
    send({"ready": True})

    for line in sys.stdin:
        request = json.loads(line)
        if request.get("reseed"):
            random.seed()
        try:
            result = handler(request["event"], Context(request))
            reply = json.dumps({"response": result}, allow_nan=False)
        except Exception as exc:
            reply = json.dumps({"error": describe(exc)})
        protocol.write(reply + "\n")
        protocol.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
