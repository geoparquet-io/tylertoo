"""Pytest configuration for tylertoo tests."""


def pytest_configure(config):
    """Register custom markers."""
    config.addinivalue_line(
        "markers",
        "integration: mark test as integration test (may require fixtures)",
    )
