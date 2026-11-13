#!/usr/bin/env python3
"""
Patch OpenCode Desktop app.asar to neutralize client-side Free Limit lockout (OpenCode 2.3+).

Preserves exact byte lengths so asar archive offsets and headers remain 100% valid.
"""

import os
import sys
import shutil

ASAR_PATH = "/opt/OpenCode/resources/app.asar"
BACKUP_PATH = ASAR_PATH + ".backup-orig"

def main():
    if not os.path.exists(ASAR_PATH):
        print(f"Error: {ASAR_PATH} not found.")
        sys.exit(1)

    if os.geteuid() != 0:
        print("Error: This script must be run with sudo/root privileges to modify app.asar.")
        print(f"Usage: sudo {sys.argv[0]}")
        sys.exit(1)

    # 1. Ensure backup exists
    if not os.path.exists(BACKUP_PATH):
        print(f"Creating backup: {BACKUP_PATH}")
        shutil.copyfile(ASAR_PATH, BACKUP_PATH)

    with open(ASAR_PATH, "rb") as f:
        data = f.read()

    # Target 1: Make freeLimit.active() always return false
    # Original: return!e()&&t!==null&&t>i() (27 bytes)
    # Patched:  return !1 &&t!==null&&t>i() (27 bytes)
    t1_old = b"return!e()&&t!==null&&t>i()"
    t1_new = b"return !1 &&t!==null&&t>i()"

    # Target 2: Neutralize resetAt state setter
    # Original: e.data.error.type===`provider.free-limit`&&r(`resetAt` (55 bytes)
    # Patched:  e.data.error.type===`provider.free-xxxxx`&&r(`resetAt` (55 bytes)
    t2_old = b"e.data.error.type===`provider.free-limit`&&r(`resetAt`"
    t2_new = b"e.data.error.type===`provider.free-xxxxx`&&r(`resetAt`"

    # Target 3: Neutralize upsell modal dialog trigger
    # Original: if(t.type===`provider.free-limit`)return h(m,e); (47 bytes)
    # Patched:  if(t.type===`provider.free-xxxxx`)return h(m,e); (47 bytes)
    t3_old = b"if(t.type===`provider.free-limit`)return h(m,e);"
    t3_new = b"if(t.type===`provider.free-xxxxx`)return h(m,e);"

    # Check if already patched
    if data.count(t1_new) == 1 and data.count(t2_new) == 1 and data.count(t3_new) == 1:
        print("OpenCode app.asar is already patched!")
        sys.exit(0)

    assert data.count(t1_old) == 1, "Target 1 not found or ambiguous"
    assert data.count(t2_old) == 1, "Target 2 not found or ambiguous"
    assert data.count(t3_old) == 1, "Target 3 not found or ambiguous"

    data = data.replace(t1_old, t1_new)
    data = data.replace(t2_old, t2_new)
    data = data.replace(t3_old, t3_new)

    with open(ASAR_PATH, "wb") as f:
        f.write(data)

    print("Success: OpenCode app.asar successfully patched!")
    print("Restart OpenCode Desktop or press Ctrl+R to reload.")

if __name__ == "__main__":
    main()
