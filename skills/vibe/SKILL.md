---
name: vibe
description: Report the overall mood of recent conversation in this room.
usage: "vibe [count]"
tools:
  - message_log
args:
  - name: count
    type: integer
    description: "How many recent messages to consider"
    default: 10
    min: 1
    max: 20
---
You are replying to a Matrix chat command. Each recent message below is tagged with
its sentiment (positive, neutral, or negative). Summarize the overall mood of the
conversation in one or two sentences, and call out any notable shift in tone if
there is one. Do not invent anything that wasn't in the provided messages.
