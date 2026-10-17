import os
import random
import sys
import time

invocations = 0
print("python example initialized", file=sys.stderr)


def handler(event, context):
    global invocations
    invocations += 1
    action = event.get("action", "echo") if isinstance(event, dict) else "echo"
    if action == "echo":
        return {"echo": event, "invocations": invocations, "pid": os.getpid(),
                "request_id": context.request_id}
    if action == "sleep":
        time.sleep(event.get("ms", 0) / 1000)
        return {"slept_ms": event.get("ms", 0)}
    if action == "random":
        return {"random": os.urandom(32).hex(), "python_random": random.random()}
    if action == "time":
        return {"unix_ms": int(time.time() * 1000)}
    if action == "env":
        return {"env": dict(os.environ)}
    if action == "files":
        path = event.get("path", "/")
        try:
            return {"path": path, "entries": sorted(os.listdir(path))}
        except OSError as exc:
            return {"path": path, "entries": {"error": str(exc)}}
    if action == "log":
        for i in range(event.get("lines", 1)):
            print(f"log line {i}")
        return {"logged": event.get("lines", 1)}
    if action == "remaining":
        return {"remaining_ms": context.get_remaining_time_in_millis()}
    if action == "fail":
        raise ValueError("the function was asked to fail")
    if action == "exit":
        os._exit(7)
    if action == "crash":
        with open("/proc/sysrq-trigger", "w") as trigger:
            trigger.write("c")
        time.sleep(60)
    raise ValueError(f"unknown action {action}")
