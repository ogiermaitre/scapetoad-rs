"""Build the Swiss test dataset: G1 boundaries of 6.4.2025 joined with STATPOP 2025.

Downloads the OFS generalised boundaries (GeoPackage, LV95) and the permanent resident
population per commune from STAT-TAB, then writes communes, districts, cantons, country
and lakes in LV95 (EPSG:2056): all layers in `ch.gpkg`, and one file per layer in `shp/`,
`geojson/`, `fgb/` and `parquet/`. District and canton populations are summed from the
communes and checked against the totals STAT-TAB publishes for them.

Needs Python 3.10+ and GDAL's ogr2ogr on the PATH. Usage:

    python3 build.py [OUT_DIR]        # default: data/ch at the repository root

The output (about 20 MB) is not tracked by git; the repository keeps only this script.
"""
from csv import writer
from json import dumps, load
from pathlib import Path
from subprocess import run
from sys import argv
from tempfile import TemporaryDirectory
from urllib.request import Request, urlopen

GPKG_URL = (
    'https://data.geo.admin.ch/ch.bfs.historisierte-administrative_grenzen_g1/'
    'historisierte-administrative_grenzen_g1_2025-04-06/'
    'historisierte-administrative_grenzen_g1_2025-04-06_2056.gpkg'
)
PX_URL = 'https://www.pxweb.bfs.admin.ch/api/v1/fr/px-x-0102010000_101/px-x-0102010000_101.px'
YEAR = '2025'
SUFFIX = '_G1_20250406'

# Non-Swiss territories present in the G1 layers: Liechtenstein (70xx), Büsingen, Campione.
FOREIGN = 'c.GDENR >= 7000'

LAYERS = ('communes', 'districts', 'cantons', 'country', 'lakes')

# Per-layer copies of ch.gpkg: subdirectory -> (OGR driver, extension, creation options).
# Coordinates keep full precision and features keep their order (FlatGeobuf's spatial index
# would sort them), so every format holds the same features in the same order.
FORMATS: dict[str, tuple[str, str, list[str]]] = {
    'shp': ('ESRI Shapefile', '.shp', ['-lco', 'ENCODING=UTF-8']),
    'geojson': ('GeoJSON', '.geojson', []),
    'fgb': ('FlatGeobuf', '.fgb', ['-lco', 'SPATIAL_INDEX=NO']),
    'parquet': ('Parquet', '.parquet', []),
}


def fetch_population(px_url: str) -> dict[str, dict[str, tuple[int, int, int]]]:
    """Return {level: {code: (total, swiss, foreign)}} for communes, districts and cantons.

    Levels are told apart by the label prefix STAT-TAB uses: '......' for communes, '>>'
    for districts, '- ' for cantons. The 'Sans indication' row (asylum seekers with no
    commune) has none of these prefixes and is dropped.
    """
    with urlopen(px_url) as resp:
        meta = load(resp)
    geo_var = meta['variables'][1]
    prefixes = {'commune': '......', 'district': '>>', 'canton': '- '}
    level_of = {}
    for code, text in zip(geo_var['values'], geo_var['valueTexts']):
        for level, prefix in prefixes.items():
            if text.startswith(prefix):
                level_of[code] = level

    query = {
        'query': [
            {'code': 'Jahr', 'selection': {'filter': 'item', 'values': [YEAR]}},
            {'code': geo_var['code'], 'selection': {'filter': 'item', 'values': list(level_of)}},
            {'code': 'Bevölkerungstyp', 'selection': {'filter': 'item', 'values': ['1']}},
            {'code': 'Staatsangehörigkeit (Kategorie)',
             'selection': {'filter': 'item', 'values': ['-99999', '1', '2']}},
            {'code': 'Geschlecht', 'selection': {'filter': 'item', 'values': ['-99999']}},
            {'code': 'Alter', 'selection': {'filter': 'item', 'values': ['-99999']}},
        ],
        'response': {'format': 'json-stat2'},
    }
    req = Request(px_url, data=dumps(query).encode(), headers={'Content-Type': 'application/json'})
    with urlopen(req) as resp:
        data = load(resp)

    # json-stat2 values are row-major over the dimensions; only geo (3 x) and nationality vary.
    geo_index = data['dimension'][data['id'][1]]['category']['index']
    nat_index = data['dimension'][data['id'][3]]['category']['index']
    values = data['value']
    out: dict[str, dict[str, tuple[int, int, int]]] = {level: {} for level in prefixes}
    for code, i in geo_index.items():
        total, swiss, foreign = (values[i * 3 + nat_index[k]] for k in ('-99999', '1', '2'))
        out[level_of[code]][code] = (total, swiss, foreign)
    return out


def ogr(*args: str) -> None:
    """Run ogr2ogr, failing loudly."""
    run(['ogr2ogr', *args], check=True)


def export(gpkg: Path, out: Path, layer: str, sql: str) -> None:
    """Write the result of a SQLite-dialect query on the work GeoPackage as a layer of `out`."""
    update = ['-update'] if out.exists() else []
    ogr('-f', 'GPKG', *update, '-overwrite', '-a_srs', 'EPSG:2056',
        '-nlt', 'MULTIPOLYGON', '-nln', layer, '-dialect', 'SQLite', '-sql', sql,
        str(out), str(gpkg))


def convert(gpkg: Path, out_dir: Path) -> None:
    """Copy every layer of `gpkg` to one file per layer in each format of FORMATS."""
    for sub, (driver, ext, options) in FORMATS.items():
        (out_dir / sub).mkdir(exist_ok=True)
        for layer in LAYERS:
            out = out_dir / sub / f'{layer}{ext}'
            # A Shapefile is several files; remove its sidecars too.
            for old in out.parent.glob(f'{layer}.*'):
                old.unlink()
            ogr('-f', driver, *options, '-nln', layer, str(out), str(gpkg), layer)


def check_sums(gpkg: Path, pop: dict[str, dict[str, tuple[int, int, int]]]) -> None:
    """Compare district and canton sums of the commune table with STAT-TAB's own totals."""
    res = run(['ogrinfo', '-q', '-dialect', 'SQLite', '-sql',
               'SELECT p.KTKZ, p.BEZNR, sum(p.POP) AS POP FROM pop p GROUP BY p.KTKZ, p.BEZNR',
               str(gpkg)], check=True, capture_output=True, text=True).stdout
    by_district: dict[int, int] = {}
    by_canton: dict[str, int] = {}
    kt = bez = None
    for line in res.splitlines():
        line = line.strip()
        if line.startswith('KTKZ'):
            kt = line.split('= ')[1]
        elif line.startswith('BEZNR'):
            bez = int(line.split('= ')[1])
        elif line.startswith('POP'):
            n = int(line.split('= ')[1])
            by_district[bez] = n
            by_canton[kt] = by_canton.get(kt, 0) + n
    bad = [c for c, v in pop['district'].items() if by_district.get(int(c)) != v[0]]
    bad += [c for c, v in pop['canton'].items() if by_canton.get(c) != v[0]]
    if bad:
        raise SystemExit(f'commune sums differ from STAT-TAB totals for: {bad}')


def main() -> None:
    out_dir = Path(argv[1]) if len(argv) > 1 else Path(__file__).resolve().parents[2] / 'data' / 'ch'
    out_dir.mkdir(parents=True, exist_ok=True)
    pop = fetch_population(PX_URL)

    with TemporaryDirectory() as tmp:
        gpkg = Path(tmp, 'g1.gpkg')
        with urlopen(GPKG_URL) as resp:
            gpkg.write_bytes(resp.read())

        # Commune table, with district and canton keys taken from the G1 communes layer.
        csv_path = Path(tmp, 'pop.csv')
        with csv_path.open('w', newline='') as fh:
            w = writer(fh)
            w.writerow(['GDENR', 'POP', 'POP_CH', 'POP_FOR'])
            for code, (total, swiss, foreign) in pop['commune'].items():
                w.writerow([int(code), total, swiss, foreign])
        ogr('-update', '-nln', 'pop_raw', '-oo', 'AUTODETECT_TYPE=YES', str(gpkg), str(csv_path))
        ogr('-update', '-nln', 'pop', '-dialect', 'SQLite', '-sql',
            f'SELECT c.GDENR, c.BEZNR, c.KTKZ, r.POP, r.POP_CH, r.POP_FOR '
            f'FROM Communes{SUFFIX} c JOIN pop_raw r ON r.GDENR = c.GDENR',
            str(gpkg), str(gpkg))

        joined = int(run(['ogrinfo', '-q', '-dialect', 'SQLite', '-sql',
                          'SELECT count(*) AS n FROM pop', str(gpkg)],
                         check=True, capture_output=True, text=True).stdout.split('= ')[-1])
        if joined != len(pop['commune']):
            raise SystemExit(f'{len(pop["commune"]) - joined} communes have no geometry')
        check_sums(gpkg, pop)

        canton_names = Path(tmp, 'kt.csv')
        with canton_names.open('w', newline='') as fh:
            w = writer(fh)
            w.writerow(['KTKZ', 'KTNAME'])
            meta_names = fetch_canton_names(PX_URL)
            w.writerows(sorted(meta_names.items()))
        ogr('-update', '-nln', 'kt_names', str(gpkg), str(canton_names))

        out = out_dir / 'ch.gpkg'
        out.unlink(missing_ok=True)
        # Communal forests (GARTE 12) have no residents: kept with POP = 0 so the map has no holes.
        export(gpkg, out, 'communes', f"""
            SELECT c.GDENR, c.GDENAME, c.GARTE, c.BEZNR, c.KTNR, c.KTKZ, c.AREA_HA,
                   coalesce(r.POP, 0) AS POP, coalesce(r.POP_CH, 0) AS POP_CH,
                   coalesce(r.POP_FOR, 0) AS POP_FOR,
                   round(coalesce(r.POP, 0) * 100.0 / c.AREA_HA, 1) AS DENSITY, c.geom
            FROM Communes{SUFFIX} c LEFT JOIN pop_raw r ON r.GDENR = c.GDENR
            WHERE NOT ({FOREIGN}) ORDER BY c.GDENR""")
        export(gpkg, out, 'districts', f"""
            SELECT d.BEZNR, d.BEZNAME, d.KTNR, d.KTKZ, d.AREA_HA,
                   s.POP, s.POP_CH, s.POP_FOR, round(s.POP * 100.0 / d.AREA_HA, 1) AS DENSITY, d.geom
            FROM Districts{SUFFIX} d JOIN (
                SELECT BEZNR, sum(POP) AS POP, sum(POP_CH) AS POP_CH, sum(POP_FOR) AS POP_FOR
                FROM pop GROUP BY BEZNR) s ON s.BEZNR = d.BEZNR
            ORDER BY d.BEZNR""")
        export(gpkg, out, 'cantons', f"""
            SELECT k.KTNR, k.KTKZ, n.KTNAME, k.AREA_HA,
                   s.POP, s.POP_CH, s.POP_FOR, round(s.POP * 100.0 / k.AREA_HA, 1) AS DENSITY, k.geom
            FROM Cantons{SUFFIX} k JOIN kt_names n ON n.KTKZ = k.KTKZ JOIN (
                SELECT KTKZ, sum(POP) AS POP, sum(POP_CH) AS POP_CH, sum(POP_FOR) AS POP_FOR
                FROM pop GROUP BY KTKZ) s ON s.KTKZ = k.KTKZ
            ORDER BY k.KTNR""")
        export(gpkg, out, 'country',
               f"SELECT CODE_ISO, AREA_HA, geom FROM Country{SUFFIX} WHERE CODE_ISO = 'CH'")
        export(gpkg, out, 'lakes',
               f'SELECT SEENR, SEENAME, AREA_HA, geom FROM Lacs{SUFFIX} ORDER BY SEENR')
        convert(out, out_dir)


def fetch_canton_names(px_url: str) -> dict[str, str]:
    """Canton names as STAT-TAB writes them ('- Zürich' → 'Zürich'), keyed by abbreviation."""
    with urlopen(px_url) as resp:
        geo_var = load(resp)['variables'][1]
    return {code: text[2:] for code, text in zip(geo_var['values'], geo_var['valueTexts'])
            if text.startswith('- ')}


if __name__ == '__main__':
    main()
