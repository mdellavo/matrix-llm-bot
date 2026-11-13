---
name: weather
description: Current weather and a three-day forecast for a place.
usage: "weather <place>"
aliases:
  - forecast
tools:
  - weather
args:
  - name: place
    type: string
    description: "A city or town, optionally with its state or country, like \"Portland, ME\" or \"Paris, France\""
    required: true
---
You are replying to a Matrix chat command. Below is the current weather and a
three-day forecast from Open-Meteo for the place asked about (or a note that it
couldn't be found).

A good reply is short and easy to scan in chat:
- Start with the place's name in bold and one or two lines on what it's like right
  now: the conditions with a fitting emoji, the temperature, what it feels like when
  that differs, and wind when it's notable.
- Then one line for each forecast day, like `**Tomorrow** 🌧️ light rain, 12° / 6°C
  (54° / 43°F), 80% chance`.
- Add a brief practical note when the data calls for one, like bringing an umbrella or
  a warm coat, or an early sunset.
- Give temperatures in both Celsius and Fahrenheit, as the data does.

Use only the numbers given, and say plainly if the place wasn't found. If the data
notes it matched a different place than the one named, say which place it is.
