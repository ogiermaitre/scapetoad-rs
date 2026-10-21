"""Tests of the Python bindings on ScapeToad's Swiss cantons sample."""

import pickle
import struct
from pathlib import Path

import pytest

import scapetoad as st

SAMPLE = Path(__file__).resolve().parents[3] / 'testdata' / 'scapetoad-sample'


@pytest.fixture(scope='module')
def cantons() -> st.Layer:
    return st.read_layer(SAMPLE / 'Cantons.shp')


@pytest.fixture(scope='module')
def lakes() -> st.Layer:
    return st.read_layer(SAMPLE / 'Lakes.shp')


@pytest.fixture(scope='module')
def carto(cantons: st.Layer, lakes: st.Layer) -> st.Cartogram:
    # Quality 0 (250 nodes) keeps the test fast and is enough to check the plumbing.
    return st.compute(cantons, 'Pop2006', quality=0, include=[lakes])


def test_read_layer(cantons: st.Layer) -> None:
    assert len(cantons) == 27
    assert ('Pop2006', 'int') in cantons.fields
    assert cantons.crs is None
    assert not cantons.looks_geographic()
    assert cantons.invalid_features() == []
    assert cantons.envelope is not None
    x0, y0, x1, y1 = cantons.envelope
    assert x0 < x1 and y0 < y1


def test_missing_file_raises_oserror(tmp_path: Path) -> None:
    with pytest.raises(OSError):
        st.read_layer(tmp_path / 'nope.geojson')


def test_columns(cantons: st.Layer) -> None:
    columns = cantons.columns()
    assert list(columns) == [name for name, _ in cantons.fields]
    assert columns['Pop2006'] == cantons.column('Pop2006')
    assert all(isinstance(v, int) for v in columns['Pop2006'])
    with pytest.raises(KeyError):
        cantons.column('nope')


def test_compute(carto: st.Cartogram, cantons: st.Layer) -> None:
    report = carto.report
    assert report.stop == 'converged'
    assert report.attribute == 'Pop2006'
    assert report.included_layers == ['Lakes']
    assert report.invalid_input == report.invalid_output == []
    assert abs(carto.errors.mean - 100) < 5
    assert len(carto.size_errors) == len(carto.masses) == len(cantons)
    assert 'CARTOGRAM COMPUTATION REPORT' in str(report)
    assert (st.SIZE_ERROR_FIELD, 'float') in carto.master.fields
    assert carto.master.column(st.SIZE_ERROR_FIELD) == pytest.approx(carto.size_errors)


def test_compute_errors(cantons: st.Layer) -> None:
    with pytest.raises(st.ScapeToadError, match='not found'):
        st.compute(cantons, 'nope', quality=0)
    with pytest.raises(ValueError):
        st.compute(cantons, 'Pop2006', quality=10, nodes=300)
    with pytest.raises(ValueError):
        st.compute(cantons, 'Pop2006', quality=101)


def test_progress(cantons: st.Layer) -> None:
    steps: list[st.Progress] = []
    st.compute(cantons, 'Pop2006', quality=0, progress=steps.append)
    stages = [s.stage for s in steps]
    assert stages[0] == 'rasterizing' and stages[-1] == 'projecting'
    diffusing = [s for s in steps if s.stage == 'diffusing']
    assert diffusing and all(s.iteration is not None for s in diffusing)


def test_progress_exception_stops_computation(cantons: st.Layer) -> None:
    calls = 0

    def stop(p: st.Progress) -> None:
        nonlocal calls
        calls += 1
        if p.stage == 'diffusing':
            raise RuntimeError('stop')

    with pytest.raises(RuntimeError, match='stop'):
        st.compute(cantons, 'Pop2006', quality=0, progress=stop)
    assert calls == 2  # rasterizing, then the first diffusion step


def test_field_round_trips(carto: st.Cartogram, tmp_path: Path) -> None:
    field = carto.field
    path = tmp_path / 'cantons.stdf'
    field.save(path)
    loaded = st.DeformationField.load(path)
    assert loaded.size == field.size
    assert loaded.envelope == field.envelope
    assert loaded.to_bytes() == field.to_bytes()
    assert pickle.loads(pickle.dumps(field)).to_bytes() == field.to_bytes()


def test_field_projection(carto: st.Cartogram, lakes: st.Layer) -> None:
    field = carto.field
    nx, ny = field.size
    assert field.node(0, 0) == pytest.approx(field.envelope[:2])
    with pytest.raises(IndexError):
        field.node(nx, 0)
    point = (600000.0, 200000.0)
    assert field.project_points([point]) == [field.project(*point)]
    moved = field.transform_layer(lakes)
    assert len(moved) == len(lakes) and moved.fields == lakes.fields
    assert field.transform_wkb(lakes.to_wkb()) == moved.to_wkb()


def test_identity_field_keeps_points() -> None:
    field = st.DeformationField.identity((0.0, 0.0, 10.0, 10.0), 11, 11)
    assert field.project(3.3, 7.1) == pytest.approx((3.3, 7.1))


def test_legend_and_grid(carto: st.Cartogram) -> None:
    legend = carto.legend([100_000, 1_000_000])
    assert len(legend) == 2
    assert legend.column('VALUE') == [100_000.0, 1_000_000.0]
    assert len(carto.field.grid(20)) > 20


def test_from_wkb_infers_types(cantons: st.Layer) -> None:
    wkb = cantons.to_wkb()[:3]
    layer = st.Layer.from_wkb(
        wkb,
        {
            'ints': [1, 2, None],
            'mixed': [1, 2.5, 3],
            'flags': [True, False, True],
            'text': ['a', 1, None],
            'empty': [None, None, None],
        },
        name='sample',
        crs=st.Crs(epsg=21781),
    )
    assert layer.fields == [
        ('ints', 'int'),
        ('mixed', 'float'),
        ('flags', 'bool'),
        ('text', 'str'),
        ('empty', 'str'),
    ]
    assert layer.column('mixed') == [1.0, 2.5, 3.0]
    assert layer.column('text') == ['a', '1', None]
    assert layer.crs == st.Crs(epsg=21781)
    assert layer.to_wkb() == wkb


def test_from_wkb_rejects_bad_input(cantons: st.Layer) -> None:
    wkb = cantons.to_wkb()[:2]
    with pytest.raises(ValueError, match='2 geometries'):
        st.Layer.from_wkb(wkb, {'a': [1]})
    with pytest.raises(ValueError, match='index 1'):
        st.Layer.from_wkb([wkb[0], b'garbage'])
    with pytest.raises(TypeError):
        st.Layer.from_wkb(wkb, {'a': [1, object()]})  # type: ignore[list-item]


@pytest.mark.parametrize('ext', ['geojson', 'fgb', 'shp'])
def test_write_and_read_back(carto: st.Cartogram, tmp_path: Path, ext: str) -> None:
    # Every format keeps the feature order, which size_errors and the report rely on.
    out = tmp_path / f'cantons.{ext}'
    carto.master.write(out)
    back = st.read_layer(out)
    assert len(back) == len(carto.master)
    assert back.column('Pop2006') == carto.master.column('Pop2006')
    # The sample has no .prj and LV03 coordinates: no format may make up a CRS.
    assert back.crs is None


def test_write_layers_topojson(carto: st.Cartogram, lakes: st.Layer, tmp_path: Path) -> None:
    out = tmp_path / 'ch.topojson'
    st.write_layers([carto.master, carto.field.transform_layer(lakes)], out, quantize=100_000)
    assert len(st.read_layer(f'{out}#Lakes')) == len(lakes)


def test_geodataframe_round_trip(cantons: st.Layer) -> None:
    gpd = pytest.importorskip('geopandas')
    gdf = st.to_geodataframe(cantons)
    assert isinstance(gdf, gpd.GeoDataFrame)
    assert len(gdf) == len(cantons)
    back = st.from_geodataframe(gdf, name='Cantons')
    assert back.fields == cantons.fields
    assert back.to_wkb() == cantons.to_wkb()


def test_geodataframe_dtypes_and_crs(cantons: st.Layer) -> None:
    gpd = pytest.importorskip('geopandas')
    import numpy as np
    import pandas as pd

    gdf = st.to_geodataframe(cantons).iloc[:3].set_crs(21781)
    gdf['float_nan'] = [1.5, np.nan, 2.0]
    gdf['nullable_int'] = pd.array([1, None, 3], dtype='Int64')
    gdf['flag'] = np.array([True, False, True])
    gdf['when'] = pd.to_datetime(['2024-01-01', None, '2024-03-01'])
    gdf['category'] = pd.Categorical(['a', 'b', 'a'])
    layer = st.from_geodataframe(gdf)
    types = dict(layer.fields)
    assert types['float_nan'] == 'float' and layer.column('float_nan') == [1.5, None, 2.0]
    assert types['nullable_int'] == 'int' and layer.column('nullable_int') == [1, None, 3]
    assert types['flag'] == 'bool'
    assert layer.column('when') == ['2024-01-01T00:00:00', None, '2024-03-01T00:00:00']
    assert layer.column('category') == ['a', 'b', 'a']
    assert layer.crs is not None and layer.crs.epsg == 21781
    assert st.to_geodataframe(layer).crs == gpd.GeoDataFrame(geometry=[], crs=21781).crs


def test_transform_geodataframe_keeps_index(carto: st.Cartogram, lakes: st.Layer) -> None:
    pytest.importorskip('geopandas')
    gdf = st.to_geodataframe(lakes)
    gdf.index = gdf.index + 100
    moved = st.transform_geodataframe(carto.field, gdf)
    assert list(moved.index) == list(gdf.index)
    assert not moved.geometry.geom_equals(gdf.geometry).any()
    assert list(moved.geometry.to_wkb()) == carto.field.transform_layer(lakes).to_wkb()


def test_corrupt_field_raises_scapetoad_error(carto: st.Cartogram) -> None:
    data = carto.field.to_bytes()
    # Grid size at bytes 12..20: a size that overflows must not panic (PanicException).
    oversized = data[:12] + struct.pack('<II', 0xFFFFFFFF, 0xFFFFFFFF) + data[20:]
    for bad in (data[:30], data[:-1], oversized):
        with pytest.raises(st.ScapeToadError):
            st.DeformationField.from_bytes(bad)


@pytest.mark.parametrize(
    'option', [{'margin': -0.6}, {'margin': float('nan')}, {'bias': 0.0}, {'tolerance': -1.0}]
)
def test_options_out_of_range_raise_value_error(cantons: st.Layer, option: dict[str, float]) -> None:
    with pytest.raises(ValueError, match=next(iter(option))):
        st.compute(cantons, 'Pop2006', quality=0, **option)  # type: ignore[arg-type]


def test_features_without_a_number_are_reported(cantons: st.Layer) -> None:
    pop: list[str | None] = [str(v) for v in cantons.column('Pop2006')]
    pop[1:4] = ["1'234", '12,5', None]
    layer = st.Layer.from_wkb(cantons.to_wkb(), {'pop': pop})
    carto = st.compute(layer, 'pop', quality=0)
    assert carto.report.no_value == [1, 2, 3]
    assert carto.masses[1:4] == [0.0, 0.0, 0.0]
    assert 'without a numeric value' in str(carto.report)
