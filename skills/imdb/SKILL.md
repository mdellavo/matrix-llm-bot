---
name: imdb
description: Look up a movie or show on IMDb.
usage: "imdb <title>"
tools:
  - imdb_lookup
args:
  - name: title
    type: string
    description: "The movie or show title to look up"
    required: true
---
You are replying to a Matrix chat command. Below is an IMDb lookup result for the
requested title (or a note that it couldn't be found). Summarize the title, year,
and a short plot line if given, and include the poster and IMDb links if present.
Do not invent details beyond what was provided.
