---
name: topics
description: Show what's been discussed recently in this room.
usage: "topics [count]"
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
You are replying to a Matrix chat command. Each recent message below may be tagged
with entities it mentioned, including topics. List the main topics and themes that
have come up, as a short bullet list. Do not invent anything that wasn't in the
provided messages.
