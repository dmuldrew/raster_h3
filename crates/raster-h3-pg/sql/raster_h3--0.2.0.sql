-- complain if script is sourced in psql, rather than via CREATE EXTENSION
\echo Use "CREATE EXTENSION raster_h3" to load this file. \quit

-- ============================================================================
-- H3 Scalar Helper Functions
-- ============================================================================

CREATE OR REPLACE FUNCTION h3_to_string(cell bigint)
RETURNS text
AS 'MODULE_PATHNAME', 'pg_h3_to_string'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION string_to_h3(hex text)
RETURNS bigint
AS 'MODULE_PATHNAME', 'pg_string_to_h3'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_to_lat(cell bigint)
RETURNS double precision
AS 'MODULE_PATHNAME', 'pg_h3_to_lat'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_to_lng(cell bigint)
RETURNS double precision
AS 'MODULE_PATHNAME', 'pg_h3_to_lng'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_get_resolution(cell bigint)
RETURNS bigint
AS 'MODULE_PATHNAME', 'pg_h3_get_resolution'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_is_valid(cell bigint)
RETURNS boolean
AS 'MODULE_PATHNAME', 'pg_h3_is_valid'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_is_valid(hex text)
RETURNS boolean
AS 'MODULE_PATHNAME', 'pg_h3_is_valid_text'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_to_wkb(cell bigint)
RETURNS bytea
AS 'MODULE_PATHNAME', 'pg_h3_to_wkb'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_cell_to_parent(cell bigint, parent_res bigint)
RETURNS bigint
AS 'MODULE_PATHNAME', 'pg_h3_cell_to_parent'
LANGUAGE C IMMUTABLE STRICT PARALLEL SAFE;

-- ============================================================================
-- Set-Returning Functions (SRFs) for Raster Aggregation
-- ============================================================================

CREATE OR REPLACE FUNCTION h3_raster_continuous_aggregate(
    raster_path text,
    h3_res integer DEFAULT 8,
    source_crs text DEFAULT NULL,
    nodata double precision DEFAULT NULL,
    bbox text DEFAULT NULL
)
RETURNS TABLE (
    h3_index bigint,
    h3_hex text,
    mean double precision,
    stddev double precision,
    count double precision,
    min double precision,
    max double precision,
    sum double precision,
    resolution smallint,
    wkb bytea
)
AS 'MODULE_PATHNAME', 'pg_h3_raster_continuous_aggregate'
LANGUAGE C VOLATILE PARALLEL SAFE;

CREATE OR REPLACE FUNCTION h3_raster_categorical_aggregate(
    raster_path text,
    h3_res integer DEFAULT 8,
    source_crs text DEFAULT NULL,
    nodata double precision DEFAULT NULL,
    bbox text DEFAULT NULL
)
RETURNS TABLE (
    h3_index bigint,
    h3_hex text,
    majority_class bigint,
    majority_fraction double precision,
    majority_count double precision,
    unique_classes bigint,
    total_count double precision,
    histogram text,
    resolution smallint,
    shannon_entropy double precision,
    wkb bytea
)
AS 'MODULE_PATHNAME', 'pg_h3_raster_categorical_aggregate'
LANGUAGE C VOLATILE PARALLEL SAFE;
