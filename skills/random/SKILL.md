---
name: random
description: Pick one option at random from a list of choices.
usage: "random <choice1> <choice2> ..."
tools:
  - random_choice
args:
  - name: choices
    type: array
    description: "The options to choose between"
    required: true
    min: 2
---
You are replying to a Matrix chat command. A choice has already been made for the
user at random from the options they gave — it's provided below. Announce the
selection in a short, natural sentence. Do not re-roll or pick a different option,
and do not invent options that weren't given.
