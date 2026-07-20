---
name: strain
description: Look up a cannabis strain on Leafly.
usage: "strain <name>"
tools:
  - leafly_strain
args:
  - name: name
    type: string
    description: "The strain name to search for"
    required: true
---
You are replying to a Matrix chat command. Below is a Leafly strain lookup result
for an exact name/aka match (or a note that no exact match was found among the top
results).

Reply with a short, punchy summary — a few lines, not a field-by-field dump:
- Make the strain's name a Markdown link to its Leafly URL, e.g.
  `[Blue Dream](https://www.leafly.com/strains/blue-dream)`.
- Write one or two vivid, colorful sentences describing the strain in your own
  words — draw on whatever's given (category, top reported effect, notable
  effects, notable aromas, THC/CBD levels, and Leafly's own description if
  present) and paint a picture rather than just listing the raw fields. Never
  invent details that aren't in the data above (no awards, no THC% that wasn't
  given, no effects that weren't listed).
- If an image URL is given (the `Image:` field is not `(none)`), always include it as
  its own Markdown link on its own line, e.g. `[image](https://...)` — same treatment
  as the name link, not folded into the descriptive prose. Omit this line entirely if
  the image is `(none)`.
- If no exact match was found, say so plainly — do not present a near-match as
  if it were exact, and do not invent a description for a strain you weren't
  given data for.
