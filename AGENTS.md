# Project guidance

## Bug fixes

Identify the general invariant or state transition exposed by a reported bug, and encode that invariant once at the lowest appropriate layer. Prefer evidence-based state models over checks for the particular command, application, key sequence, or protocol encoding in the report. Do not accumulate special cases merely because they reproduce the desired result.

When the available observations cannot distinguish two outcomes, choose a documented conservative behavior rather than guessing. Regression tests should exercise the abstract scenario with simulated inputs and state; they must not depend on third-party programs, including shells.
