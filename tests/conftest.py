import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1] / "PrototypeScarlett"
sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(ROOT))

import atexit  # noqa: E402
import os  # noqa: E402
import shutil  # noqa: E402
import tempfile  # noqa: E402

import pytest  # noqa: E402


def _dossier_de_test(variable, prefixe):
    """Un dossier temporaire propre à ce processus de tests sous `variable`, effacé à la sortie. Celui que
    l'utilisateur a donné est gardé ; celui hérité du processus qui a lancé ce worker de pytest-xdist est remplacé
    (les workers ne partagent pas de dossier)."""
    marque, pid = f"{variable}_PYTEST_PID", str(os.getpid())
    if variable in os.environ and os.environ.get(marque, pid) == pid:
        return
    dossier = tempfile.mkdtemp(prefix=prefixe)
    atexit.register(shutil.rmtree, dossier, True)
    os.environ[variable], os.environ[marque] = dossier, pid


# Tests déterministes : El1xr gère seul le runtime GDK, même si Gaming Services est
# installé. Les tests de délégation lancent un sous-processus avec la valeur "1".
os.environ.setdefault("ANYXBOXX_SYSTEM_RUNTIME", "0")
# Un test qui passe par dev.py run_bench ne tue jamais les builds de la machine pour faire de la place
# en mémoire engagée (dev_throttle.make_room) : il n'en demande aucune.
os.environ["ANYXBOXX_GAME_COMMIT_GB"] = "0"
# Cache de shaders propre à la session de tests : jamais celui de l'utilisateur.
_dossier_de_test("ANYXBOXX_SHADER_CACHE", "anyxboxx-shader-cache-")
# Sans DLL de TomBox dans les tests du runtime : ils vérifient nos DLL. Le dossier par défaut
# (runtime/build/tombox) est testé à part, et tests/test_tombox_native.py le désigne explicitement.
_dossier_de_test("ANYXBOXX_TOMBOX_DIR", "anyxboxx-sans-tombox-")

RUNTIME_DIR = Path(os.environ.get("ANYXBOXX_RUNTIME")
                   or Path(__file__).resolve().parents[1] / "runtime" / "build" / "bin")
RUNTIME_DLLS = ("xgameruntime.dll", "xmem.dll", "d3d12_xs.dll", "kernelx.dll", "anyxboxx_tls.dll", "XCRDAPI.dll")


def runtime_built():
    return all((RUNTIME_DIR / name).is_file() for name in (*RUNTIME_DLLS, "anyxboxx.exe"))


@pytest.fixture
def runtime_dir():
    """Dossier du runtime C++ ; absent : test ignoré, sauf en CI (ANYXBOXX_REQUIRE_RUNTIME=1)."""
    if not runtime_built():
        if os.environ.get("ANYXBOXX_REQUIRE_RUNTIME") == "1":
            pytest.fail(f"Runtime C++ absent de {RUNTIME_DIR} : compilez runtime/ avec CMake.")
        pytest.skip("runtime C++ non compilé (cmake -S runtime -B runtime/build)")
    return RUNTIME_DIR
