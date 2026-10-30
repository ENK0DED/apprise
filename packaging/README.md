## Packaging
This directory contains any supporting files to grant usage of Apprise in various distributions.

This port does not build distribution packages yet; see the main
[README](../README.md#installation) for Cargo, Docker and Node.js installs.

## Man Pages Information
The man page was generated using [Ronn-NG](https://github.com/apjanke/ronn-ng).
 - Content is directly written to entries in the **man/\*.md** files _following the
   format structure described in `ronn-format(7)`_.
 - Then the following is executed from this directory to produce the
   man/apprise.1 (used by distributions) and man/apprise.1.html:
   ```bash
   ronn --roff --html man/apprise.md
   ```

Nothing in the build regenerates these files, so rebuild them by hand after
changing **man/apprise.md**.
