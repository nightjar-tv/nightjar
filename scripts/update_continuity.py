#!/usr/bin/env python3
"""
Automatically update CONTINUITY.md after PR merge.
"""

import os
import subprocess
import json
from datetime import datetime

# Paths
CONTINUITY_PATH = "../nightjar-meta/docs/CONTINUITY.md"
GIT_STATUS_PATH = "git_status.json"

# Get current git status
def get_git_status():
    try:
        result = subprocess.run([
            "git", "status", "--porcelain"
        ], capture_output=True, text=True, check=True)
        return result.stdout.strip()
    except subprocess.CalledProcessError as e:
        print(f"Error getting git status: {e}")
        return ""

# Get current branch
def get_current_branch():
    try:
        result = subprocess.run([
            "git", "rev-parse", "--abbrev-ref", "HEAD"
        ], capture_output=True, text=True, check=True)
        return result.stdout.strip()
    except subprocess.CalledProcessError as e:
        print(f"Error getting branch: {e}")
        return "unknown"

# Get current tip
def get_current_tip():
    try:
        result = subprocess.run([
            "git", "rev-parse", "HEAD"
        ], capture_output=True, text=True, check=True)
        return result.stdout.strip()
    except subprocess.CalledProcessError as e:
        print(f"Error getting tip: {e}")
        return "unknown"

# Update CONTINUITY.md
def update_continuity():
    # Get current state
    status = get_git_status()
    branch = get_current_branch()
    tip = get_current_tip()
    timestamp = datetime.now().isoformat()

    # Create update content
    update_content = f"""# CONTINUITY.md

## Current State

- **Branch:** {branch}
- **Tip:** {tip}
- **Uncommitted work:**
"""

    # Add uncommitted files
    if status:
        update_content += "  - Uncommitted changes:\n"
        for line in status.split("\n"):
            if line.strip():
                status_code = line[:2]
                file_path = line[3:]
                if status_code == "M " or status_code == "A " or status_code == "D ":
                    update_content += f"    - {file_path}\n"
    else:
        update_content += "  - Nothing critical (all work committed)\n"

    # Add timestamp
    update_content += f"\n- **Last updated:** {timestamp}\n"

    # Write to file
    try:
        with open(CONTINUITY_PATH, "w") as f:
            f.write(update_content)
        print(f"Successfully updated {CONTINUITY_PATH}")
        return True
    except Exception as e:
        print(f"Error writing to {CONTINUITY_PATH}: {e}")
        return False

# Main execution
if __name__ == "__main__":
    success = update_continuity()
    if not success:
        exit(1)
    exit(0)