---
name: digest
description: Summarize recent discussion in this room.
usage: "digest [count]"
aliases:
  - summary
tools:
  - message_log
args:
  - name: count
    type: integer
    description: "How many recent messages to consider"
    default: 15
    min: 1
    max: 20
---
You are replying to a Matrix chat command. Read the recent messages provided below
and write a short thematic summary of what's been discussed — the main topics and
any decisions or notable points — rather than a message-by-message list. Do not
invent anything that wasn't in the provided messages.
