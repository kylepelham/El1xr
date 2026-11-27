"""Les 9 README (anglais + 8 traductions) restent synchronisés (règle 6 d'AGENTS.md)."""
import re
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[1]
READMES = [REPO / "README.md", *sorted((REPO / "docs" / "i18n").glob("README.*.md"))]
LANGUAGE_COUNT = 9
# Les README annoncent une centaine suivie de « + » (806 tests -> « 800+ ») : une borne basse, qui
# reste vraie tant que la suite compte au moins ce nombre. L'annonce peut retarder d'une centaine
# (1700+ pour 1733 ou 1899 tests) : une PR qui ajoute des tests ne touche les 9 README que si la
# suite dépasse de deux centaines l'annonce, sinon toutes les PR ouvertes en même temps (des
# dizaines de branches en parallèle) entrent en conflit sur cette seule ligne.
TEST_COUNT_STEP = 100
TEST_COUNT_LAG_STEPS = 1
# Libellé de la ligne « N+ tests automatisés » dans chaque langue.
TEST_COUNT_ROW = re.compile(
    r"^\| *(\d+)\+ *(?:automated tests|automatisierte Tests|pruebas automatizadas|tests automatisés"
    r"|test automatizzati|件の自動テスト|testes automatizados|个自动化测试|автоматических тестов) *\|", re.M)
IMAGE = re.compile(r"\(([^()\s]*\.(?:svg|png))\)")


def rounded_test_count(collected):
    """Nombre à écrire dans les README pour `collected` tests : la centaine inférieure."""
    return collected - collected % TEST_COUNT_STEP


def claim_holds(claimed, collected):
    """L'annonce `claimed`+ convient pour `collected` tests : une centaine, au plus la centaine
    inférieure, au moins TEST_COUNT_LAG_STEPS centaines en dessous."""
    expected = rounded_test_count(collected)
    return claimed % TEST_COUNT_STEP == 0 and expected - TEST_COUNT_LAG_STEPS * TEST_COUNT_STEP <= claimed <= expected


def table_row_count(text):
    return sum(1 for line in text.splitlines() if line.startswith("|"))


def images(text):
    return sorted(Path(path).name for path in IMAGE.findall(text))


def claimed_test_count(text):
    found = TEST_COUNT_ROW.findall(text)
    assert len(found) == 1, f"ligne « N+ tests automatisés » trouvée {len(found)} fois"
    return int(found[0])


@pytest.fixture(scope="module")
def texts():
    return {path.relative_to(REPO).as_posix(): path.read_text(encoding="utf-8") for path in READMES}


def test_nine_languages(texts):
    assert len(texts) == LANGUAGE_COUNT, sorted(texts)


@pytest.mark.parametrize("measure", [table_row_count, images, claimed_test_count],
                         ids=["lignes-de-tableau", "images", "nombre-de-tests"])
def test_same_content_in_every_language(texts, measure):
    reference = measure(texts["README.md"])
    for name, text in texts.items():
        assert measure(text) == reference, f"{name} diffère de README.md ({measure.__name__})"


def full_suite_collected(config):
    """Vrai seulement pour un lancement complet : sans -k, sans -m, sans chemin choisi."""
    default_args = ([], [str(REPO / "tests")], ["tests"])
    return (not config.option.keyword and not config.option.markexpr
            and config.args in default_args)


def test_claimed_count_matches_suite(request, texts):
    if not full_suite_collected(request.config):
        pytest.skip("suite partielle : le nombre de tests n'est comparé que sur un lancement complet")
    collected = len(request.session.items)
    claimed = claimed_test_count(texts["README.md"])
    expected = rounded_test_count(collected)
    assert claim_holds(claimed, collected), (f"les README annoncent {claimed}+ tests, la suite en compte {collected} : "
                                             f"écrire « {expected}+ » dans les 9 README")


@pytest.mark.parametrize("collected, announced", [(806, 800), (800, 800), (899, 800), (900, 900), (99, 0)])
def test_rounded_test_count(collected, announced):
    assert rounded_test_count(collected) == announced


@pytest.mark.parametrize("claimed, collected, holds", [
    (1700, 1733, True), (1600, 1733, True), (1700, 1899, True), (1700, 1900, False), (1800, 1900, True),
    (1800, 1733, False), (1500, 1733, False), (1650, 1733, False), (0, 99, True),
])
def test_claim_may_lag_one_step(claimed, collected, holds):
    assert claim_holds(claimed, collected) is holds
