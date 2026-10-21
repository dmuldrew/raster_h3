# Security Policy

## Supported versions

Security fixes land on `main` and ship in the next release. Only the latest released
version receives fixes.

## Reporting a vulnerability

Please **do not** open a public issue for security problems.

Report privately through GitHub's
[private vulnerability reporting](https://github.com/dmuldrew/raster_h3/security/advisories/new)
(Security tab → "Report a vulnerability"). Include:

- the affected engine (DuckDB, PostgreSQL, Python/Polars, DataFusion, CLI) and version;
- a minimal reproduction (SQL, Python snippet, or raster file);
- the impact you observed (crash, memory corruption, out-of-bounds read, file or network access).

You should get an acknowledgement within 7 days. Once a fix is ready we will publish a
GitHub Security Advisory and credit you unless you prefer otherwise.

## Scope

raster_h3 is loaded into host processes (DuckDB, PostgreSQL backends, Python) and parses
untrusted input: GeoTIFF/COG files, remote HTTP range responses, WKB geometries and SQL
arguments. Bugs that let such input crash the host, read or write memory out of bounds,
or reach files or URLs the caller did not request are in scope.
