+++
title = "DT Bindings"
paths = ["Documentation/devicetree/bindings/"]
symbols = []
+++
# DT Bindings: maintainer notes

Conventions for structuring Device Tree binding YAML schemas under Documentation/devicetree/bindings/.

- If per-compatible if-then conditionals in a single binding schema become unwieldy because of variant differences, prefer splitting the binding into separate schema documents rather than piling on more conditionals.
