---
name: ud
description: Look up a word or phrase on Urban Dictionary (or a random word if none given).
usage: "ud [term]"
aliases:
  - urban
tools:
  - urban_dictionary
args:
  - name: term
    type: string
    description: "The word or phrase to define; if omitted, a random word is used"
---
You are replying to a Matrix chat command. Below is an Urban Dictionary lookup
result (or a note that none was found). Present the word and its definition
clearly and concisely. Do not invent a definition beyond what was provided, and
say plainly if nothing was found.
