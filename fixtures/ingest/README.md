# Ingest policy fixtures

`xattr-policy-v1.json` is the golden capture-policy fixture. It models one
filesystem entry carrying portable and privileged xattr namespaces, with
distinct sentinel values. Tests pass it through the same lazy value-filtering
function used by live filesystem ingest, then pin the kept names, per-entry
dropped names, namespace-widening report, and absence of values from reports.

The privileged names are fixture data rather than host xattrs because creating
`security.*`, `system.*`, and `trusted.*` attributes requires filesystem- and
privilege-specific setup. This keeps the gate deterministic without turning a
missing privilege into a silently passing test.
