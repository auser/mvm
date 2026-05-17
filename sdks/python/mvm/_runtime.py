"""Runtime-side secret substitution helpers.

This module is intentionally dependency-light: cloud SDK adapters call
``register_substitution_handler`` at credential load time, then run their
normal signing flow with the materialized credential. The actual vsock
transport lands with the W3 substitution service; tests inject handlers
directly so the contract is stable before the transport exists.
"""

from __future__ import annotations

from dataclasses import dataclass
from hmac import compare_digest
from typing import Callable, Dict


SubstitutionHandler = Callable[[str], str]


_handlers: Dict[str, SubstitutionHandler] = {}


class SubstitutionHandlerError(RuntimeError):
    """Raised when credential substitution cannot be performed."""


@dataclass(frozen=True)
class AwsCredentials:
    access_key_id: str
    secret_access_key: str
    session_token: str | None = None


def register_substitution_handler(name: str, fn: SubstitutionHandler) -> None:
    """Register a named placeholder resolver.

    ``name`` is the provider namespace (for example ``"aws"``). ``fn``
    receives one placeholder string and returns the resolved credential
    material. Handlers must run at credential loading time, before any
    request signer computes a body/header/query signature.
    """

    if not name:
        raise ValueError("substitution handler name must be non-empty")
    if not callable(fn):
        raise TypeError("substitution handler must be callable")
    _handlers[name] = fn


def clear_substitution_handlers() -> None:
    """Clear registered handlers. Public for test isolation."""

    _handlers.clear()


def substitute(name: str, placeholder: str) -> str:
    try:
        handler = _handlers[name]
    except KeyError as exc:
        raise SubstitutionHandlerError(
            f"no substitution handler registered for {name!r}"
        ) from exc
    return handler(placeholder)


def aws_credentials_from_placeholders(
    *,
    access_key_id: str,
    secret_access_key: str,
    session_token: str | None = None,
) -> AwsCredentials:
    """Resolve AWS credentials before SigV4 signing.

    Cloud SDK integrations adapt this result into their native
    credential-provider shape:

    - Python: ``botocore.credentials.Credentials``
    - TypeScript: ``@aws-sdk/credential-providers`` provider result
    - Rust: ``aws_config::SdkConfig`` credential provider
    """

    resolved_token = None
    if session_token is not None:
        resolved_token = substitute("aws", session_token)
    return AwsCredentials(
        access_key_id=substitute("aws", access_key_id),
        secret_access_key=substitute("aws", secret_access_key),
        session_token=resolved_token,
    )


def is_placeholder(value: str) -> bool:
    return value.startswith("mvm-secret://")


def constant_time_eq(left: str, right: str) -> bool:
    return compare_digest(left.encode("utf-8"), right.encode("utf-8"))
