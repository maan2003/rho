---
name: visualizations
description: Create and embed a native inline SVG visualization in rho. Use when a chart, diagram, timeline, architecture drawing, or other visual explanation would communicate better than prose alone.
---

# Native rho visualizations

Create a self-contained SVG, register its immutable bytes with the daemon, and
then reference the returned id in the final response.

## Register the SVG

Pipe the SVG to `rho record-visualization`. The command reads stdin and prints
an artifact id that can later be used as the `ref` in a visualization
fenced block:

```bash
id=$(cat <<'SVG' | rho record-visualization
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 450">
  <!-- drawing -->
</svg>
SVG
)
```

- Include a `viewBox`; `width` and `height` are optional. Rho preserves the
  SVG's aspect ratio and constrains it to the transcript.
- Use a reasonable intrinsic canvas and keep the SVG self-contained so it
  renders consistently. The daemon stores the bytes without parsing SVG or
  enforcing geometry or rendering-work limits.
- The current format is SVG only, with a 4 MiB source limit.
- Registration snapshots stdin into RhoDB. Later changes to a source file do
  not change the artifact.

## Embed it

Put this block in the final response, substituting the id and choosing its
height in transcript rows:

````text
```visualization
ref=${id} rows=18
```
````

Use the canonical field order shown above (`ref` followed by `rows`).
The `rows` field is required and must be an integer from 1 through 50.
It measures height in lines of transcript text: for example, `rows=20`
makes the visualization about as tall as 20 lines of text. Choose it
intentionally for the drawing: roughly 10-14 rows for a compact diagram,
18-24 for a detailed landscape diagram, and up to 50 for tall or dense
content.

Emit the visualization fence as raw response text, not inside another Markdown
code fence. Do not use a file path, data URL, or raw SVG in the response. Add
concise prose or a text summary when the visualization contains conclusions
the user should still be able to search or quote.
