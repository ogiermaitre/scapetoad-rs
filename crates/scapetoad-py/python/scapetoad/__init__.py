"""Diffusion-based cartograms (Gastner–Newman), a Rust port of ScapeToad.

A cartogram resizes the polygons of a map so their areas become proportional to a variable.
The computation also yields a `DeformationField` that deforms any other layer the same way.

    import scapetoad as st

    cantons = st.read_layer("Cantons.shp")
    carto = st.compute(cantons, "Pop2006", quality=50)
    print(carto.report)
    carto.master.write("cantons_cartogram.fgb")
    carto.field.save("cantons.stdf")

GeoDataFrames convert with `from_geodataframe` / `to_geodataframe` (needs geopandas).
"""

from ._scapetoad import (
    SIZE_ERROR_FIELD,
    Cartogram,
    Crs,
    DeformationField,
    ErrorSummary,
    Layer,
    Progress,
    Report,
    ScapeToadError,
    __version__,
    compute,
    read_layer,
    write_layer,
    write_layers,
)
from .interop import from_geodataframe, to_geodataframe, transform_geodataframe

__all__ = [
    'SIZE_ERROR_FIELD',
    'Cartogram',
    'Crs',
    'DeformationField',
    'ErrorSummary',
    'Layer',
    'Progress',
    'Report',
    'ScapeToadError',
    '__version__',
    'compute',
    'from_geodataframe',
    'read_layer',
    'to_geodataframe',
    'transform_geodataframe',
    'write_layer',
    'write_layers',
]
