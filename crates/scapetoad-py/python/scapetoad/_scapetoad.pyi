"""Type stubs for the native module (implemented in Rust, `crates/scapetoad-py/src`)."""

from collections.abc import Callable, Iterable, Mapping, Sequence
from os import PathLike
from typing import Any, Literal, Protocol, final, overload

from numpy import float64
from numpy.typing import NDArray

class _SupportsArray(Protocol):
    """What `project_points` treats as an array: numpy arrays, pandas and xarray objects."""

    def __array__(self) -> Any: ...

_Envelope = tuple[float, float, float, float]
"""`(min_x, min_y, max_x, max_y)`."""
_StrPath = str | PathLike[str]
"""A path; `file.gpkg#layer` names a layer in a multi-layer source."""
_Scalar = bool | int | float | str | None
_FieldTypeName = Literal['bool', 'int', 'float', 'str']

__all__ = [
    '__version__',
    'SIZE_ERROR_FIELD',
    'ScapeToadError',
    'Crs',
    'Layer',
    'DeformationField',
    'Cartogram',
    'Report',
    'ErrorSummary',
    'Progress',
    'read_layer',
    'write_layer',
    'write_layers',
    'compute',
]

__version__: str
SIZE_ERROR_FIELD: str
"""Name of the size error attribute added to the transformed master layer."""

class ScapeToadError(Exception):
    """Error from the computation or a format backend (I/O errors raise `OSError`)."""

@final
class Crs:
    """Coordinate reference system, carried through to the outputs; never reprojected."""

    def __new__(cls, epsg: int | None = None, wkt: str | None = None) -> Crs: ...
    @property
    def epsg(self) -> int | None: ...
    @property
    def wkt(self) -> str | None: ...
    @property
    def is_geographic(self) -> bool:
        """Whether the CRS is lon/lat; cartograms need planar, ideally equal-area, coordinates."""

@final
class Layer:
    """A vector layer (geometries, attribute columns, CRS) held on the Rust side."""

    @staticmethod
    def from_wkb(
        geometries: Iterable[bytes | None],
        columns: Mapping[str, Iterable[_Scalar]] | None = None,
        *,
        name: str = 'layer',
        crs: Crs | None = None,
    ) -> Layer:
        """Builds a layer from WKB geometries and attribute columns.

        Column types are inferred: int widens to float, anything mixed with text becomes text.
        """
    def write(self, target: _StrPath) -> None:
        """Writes the layer; the format comes from the extension."""
    name: str
    @property
    def fields(self) -> list[tuple[str, _FieldTypeName]]:
        """Attribute columns as `(name, type)` pairs."""
    crs: Crs | None
    """Setting it does not reproject (useful for a shapefile without `.prj`)."""
    @property
    def envelope(self) -> _Envelope | None: ...
    def looks_geographic(self) -> bool:
        """Whether every coordinate lies within ±180/±90: a hint, not a proof."""
    def invalid_features(self) -> list[int]:
        """Indices of the features whose geometry is invalid under OGC rules.

        Such polygons are rasterised by the even-odd rule and their defects carry over to the
        cartogram; repair them first (e.g. shapely's `make_valid`) for exact areas.
        """
    def column(self, name: str) -> list[_Scalar]:
        """Values of one attribute; raises `KeyError` if it does not exist."""
    def columns(self) -> dict[str, list[_Scalar]]:
        """All attributes, in column order."""
    def to_wkb(self) -> list[bytes | None]:
        """Geometries as 2D WKB."""
    def __len__(self) -> int: ...

@final
class DeformationField:
    """Deformed grid over an envelope; projects any geometry inside it like the cartogram."""

    @staticmethod
    def identity(envelope: _Envelope, nx: int, ny: int) -> DeformationField: ...
    @staticmethod
    def load(path: _StrPath) -> DeformationField:
        """Reads a `.stdf` file."""
    def save(self, path: _StrPath) -> None:
        """Writes a `.stdf` file."""
    @staticmethod
    def from_bytes(data: bytes) -> DeformationField: ...
    def to_bytes(self) -> bytes:
        """The `.stdf` encoding (also used for pickling)."""
    @property
    def envelope(self) -> _Envelope: ...
    @property
    def size(self) -> tuple[int, int]:
        """Number of nodes `(nx, ny)`."""
    @property
    def cell_size(self) -> tuple[float, float]: ...
    @property
    def crs(self) -> Crs | None: ...
    def node(self, i: int, j: int) -> tuple[float, float]:
        """Deformed position of node `(i, j)`, counted from the bottom-left corner."""
    def project(self, x: float, y: float) -> tuple[float, float]: ...
    @overload
    def project_points(self, points: _SupportsArray) -> NDArray[float64]:
        """Projects an `(n, 2)` array-like (numpy array, DataFrame…) into an `(n, 2)` array."""
    @overload
    def project_points(self, points: Iterable[Sequence[float]]) -> list[tuple[float, float]]:
        """Projects `(x, y)` pairs (tuples, lists…) into a list of tuples."""
    def transform_wkb(self, geometries: Iterable[bytes | None]) -> list[bytes | None]:
        """Projects WKB geometries, splitting segments at grid lines so they follow the deformation."""
    def transform_layer(self, layer: Layer) -> Layer:
        """Transforms every feature, keeping attributes; the layer gets the field's CRS if it has none."""
    def grid(self, lines: int = 100) -> Layer:
        """Deformed graticule with about `lines` lines along the longer side."""

@final
class ErrorSummary:
    """Size error statistics over the master features (100 = exact area)."""

    count: int
    mean: float
    std_dev: float
    p25: float
    p50: float
    p75: float
    within_one_std_dev: int

@final
class Report:
    """Parameters and outcome of a computation; `str(report)` is ScapeToad's text report."""

    layer: str
    attribute: str
    kind: Literal['mass', 'density']
    grid_size: tuple[int, int]
    zero_density: float
    bias: float
    feature_count: int
    no_value: list[int]
    """Indices of the master features without a usable number (null, unparsable text, NaN); they count as 0."""
    value_mean: float
    value_min: float
    value_max: float
    constrained_layers: list[str]
    included_layers: list[str]
    iterations: int
    stop: Literal['converged', 'max_iterations', 'timeout']
    errors: ErrorSummary
    """Size error statistics, without the `zero_value` features."""
    zero_value: list[int]
    """Indices of the master features whose mass is 0; their size error is 0 whatever their area."""
    zero_value_area: tuple[float, float]
    """Their share of the total area in percent, before and after the transformation."""
    invalid_input: list[int]
    """Indices of the master features with an invalid geometry before the transformation."""
    invalid_output: list[int]
    """Indices after the transformation; a valid input becomes invalid only where the grid folded."""
    duration: float
    """Computation time in seconds."""

@final
class Progress:
    """Passed to the `progress` callback of `compute`; numeric fields are set while diffusing."""

    stage: Literal['rasterizing', 'diffusing', 'constraining', 'projecting']
    iteration: int | None
    time: float | None
    max_displacement: float | None
    """Largest node displacement during the step, in grid cells."""
    fraction: float | None
    """Rough completion estimate in [0, 1]."""

@final
class Cartogram:
    """Result of `compute`."""

    @property
    def master(self) -> Layer:
        """Transformed master layer, with a `SizeError` attribute."""
    @property
    def field(self) -> DeformationField:
        """The deformation, reusable on any layer inside its envelope."""
    @property
    def masses(self) -> list[float]:
        """Mass of each master feature (the value, or density × area)."""
    @property
    def size_errors(self) -> list[float]:
        """Size error of each master feature: 100 × mass share / area share, 100 = exact."""
    @property
    def errors(self) -> ErrorSummary:
        """Statistics of `size_errors`, without the zero-valued features (`report.zero_value`)."""
    @property
    def report(self) -> Report: ...
    def legend(self, values: Sequence[float] | None = None) -> Layer:
        """Legend squares whose areas represent `values` at the cartogram's scale.

        By default: the two powers of ten below the largest mass, and the largest mass.
        """

def read_layer(source: _StrPath) -> Layer:
    """Reads a layer: Shapefile, GeoJSON, FlatGeobuf, TopoJSON (and GDAL formats if built with it)."""

def write_layer(layer: Layer, target: _StrPath) -> None:
    """Writes a layer, replacing an existing file (a GeoPackage gains or replaces the layer)."""

def write_layers(layers: Sequence[Layer], target: _StrPath, *, quantize: int | None = None) -> None:
    """Writes several layers into one file; in TopoJSON they share their borders.

    `quantize` rounds TopoJSON coordinates to an N×N grid over the extent.
    """

def compute(
    master: Layer,
    attribute: str,
    *,
    density: bool = False,
    quality: int | None = None,
    nodes: int | None = None,
    missing_value: float | None = None,
    margin: float | None = None,
    min_density: float | None = None,
    zero_density: float | None = None,
    bias: float | None = None,
    tolerance: float | None = None,
    max_iterations: int | None = None,
    max_duration: float | None = None,
    constrain: Sequence[Layer] | None = None,
    include: Sequence[Layer] | None = None,
    progress: Callable[[Progress], Any] | None = None,
) -> Cartogram:
    """Computes a cartogram of `master`, resizing its polygons in proportion to `attribute`.

    Args:
        density: the attribute is already a density, not a count.
        quality: 0–100, a grid of `250 + 15·quality` nodes along the longer side (default 50).
        nodes: explicit number of nodes along the longer side, instead of `quality`.
        missing_value: value marking missing data, replaced by the mean of the others.
        margin: envelope margin on each side, as a fraction of the extent (≥ 0, default 0.2).
        min_density: smallest non-zero cell density after rescaling (≥ 0, default 10).
        zero_density: density of zero-valued features, as a fraction of the smallest non-zero
            density (0–1, default 0.1). Higher converges faster but leaves them more area; 0 is
            ScapeToad's behaviour, which can stall the diffusion.
        bias: added to every cell so zero regions shrink without vanishing (> 0, default 1e-5).
        tolerance: stop when no node moves more than this, in grid cells (≥ 0, default 1e-7).
        max_iterations: cap on integration steps (default 10 000).
        max_duration: time limit in seconds (default 3 hours, `math.inf` for none).
        constrain: layers whose shapes are kept where the master layer does not cover them.
        include: layers the envelope must cover, to transform them with the field later.
        progress: called at each step. Raising from it, or Ctrl-C, stops the computation.

    Raises:
        ValueError: an option out of its range.
        ScapeToadError: the computation cannot run on this layer (attribute not found, no
            positive value, non-finite coordinates…).
    """
