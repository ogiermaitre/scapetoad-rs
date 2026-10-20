"""Conversions between `Layer` and geopandas GeoDataFrames, through WKB.

geopandas and shapely are imported lazily, so the rest of the package works without them.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from ._scapetoad import Crs, DeformationField, Layer

if TYPE_CHECKING:
    from geopandas import GeoDataFrame
    from pandas import Series


def from_geodataframe(gdf: GeoDataFrame, name: str = 'layer') -> Layer:
    """Converts a GeoDataFrame to a `Layer`, keeping its columns and CRS (not its index).

    Missing values (NaN, NaT, None) become nulls; datetime columns become ISO strings and
    other non-scalar columns their `str()`, since layers only hold bool, int, float and text.
    """
    from shapely import to_wkb

    wkb = to_wkb(gdf.geometry.values, output_dimension=2)
    columns: dict[str, list[Any]] = {
        str(column): _column_values(gdf[column])
        for column in gdf.columns
        if column != gdf.geometry.name
    }
    crs = None
    if gdf.crs is not None:
        crs = Crs(epsg=gdf.crs.to_epsg(), wkt=gdf.crs.to_wkt())
    return Layer.from_wkb(list(wkb), columns, name=name, crs=crs)


def to_geodataframe(layer: Layer) -> GeoDataFrame:
    """Converts a `Layer` to a GeoDataFrame; the CRS is set from its EPSG code, else its WKT."""
    from geopandas import GeoDataFrame
    from shapely import from_wkb

    crs = None
    if layer.crs is not None:
        crs = layer.crs.epsg or layer.crs.wkt
    return GeoDataFrame(layer.columns(), geometry=from_wkb(layer.to_wkb()), crs=crs)


def transform_geodataframe(field: DeformationField, gdf: GeoDataFrame) -> GeoDataFrame:
    """Returns a copy of `gdf` with its geometries deformed by `field`.

    Unlike a round trip through `Layer`, this keeps the index and column dtypes, which makes it
    the convenient way to carry basemaps along with a cartogram.
    """
    from shapely import from_wkb, to_wkb

    out = gdf.copy()
    wkb = field.transform_wkb(list(to_wkb(gdf.geometry.values, output_dimension=2)))
    out[gdf.geometry.name] = from_wkb(wkb)
    return out


def _column_values(series: Series) -> list[Any]:
    """Plain Python values of a column, with every kind of missing value as `None`."""
    from pandas.api.types import is_datetime64_any_dtype, is_numeric_dtype

    if is_datetime64_any_dtype(series.dtype):
        series = series.map(lambda v: v.isoformat(), na_action='ignore')
    elif not is_numeric_dtype(series.dtype):
        series = series.map(_scalar, na_action='ignore')
    values: list[Any] = series.astype(object).where(series.notna(), None).tolist()
    return values


def _scalar(value: Any) -> Any:
    """Keeps text and numbers (numpy's included) and stringifies the rest (dates, lists…)."""
    if isinstance(value, str) or hasattr(value, '__index__') or hasattr(value, '__float__'):
        return value
    return str(value)
