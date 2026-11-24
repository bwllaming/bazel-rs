import os
import platform
import sys

print("python", sys.version_info[:2], sys.platform, platform.machine())
print("runfiles", sorted(k for k in os.environ if k.startswith("RUNFILES")))
