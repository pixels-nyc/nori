#!/usr/bin/env bash

# Trigger an AI agent to configure the nested session and take a screenshot
python3 /home/super/Work/Tools\ \&\ Skills/tmux/scripts/database_mcp_server.py <<EOF
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "spawn_agent",
    "arguments": {
      "role": "QA_TESTER",
      "task_prompt": "Set WAYLAND_DISPLAY=wayland-2. Use niri-mcp to change the focus ring to a bright neon green. Take a screenshot using 'grim /tmp/niri-qa.jpg' and then view the file to verify the color change.",
      "parent_id": "system"
    }
  }
}
EOF

echo "==> QA Tester Agent Spawned! Monitor the agents database for its progress."
