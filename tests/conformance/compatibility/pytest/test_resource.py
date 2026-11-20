from pytest import fixture


@fixture
def resource():
    return 1


def test_use(resource):
    assert resource
