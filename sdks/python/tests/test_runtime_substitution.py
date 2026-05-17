import hashlib
import hmac

import mvm


def _sign(key: bytes, msg: str) -> bytes:
    return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()


def _sigv4_signature(secret: str, date: str, region: str, service: str, string_to_sign: str) -> str:
    date_key = _sign(("AWS4" + secret).encode("utf-8"), date)
    region_key = _sign(date_key, region)
    service_key = _sign(region_key, service)
    signing_key = _sign(service_key, "aws4_request")
    return hmac.new(signing_key, string_to_sign.encode("utf-8"), hashlib.sha256).hexdigest()


def test_aws_credentials_resolve_before_sigv4_signing():
    mvm.clear_substitution_handlers()
    placeholders = {
        "mvm-secret://aws/access-key": "AKIAIOSFODNN7EXAMPLE",
        "mvm-secret://aws/secret-key": "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
    }
    mvm.register_substitution_handler("aws", placeholders.__getitem__)

    creds = mvm.aws_credentials_from_placeholders(
        access_key_id="mvm-secret://aws/access-key",
        secret_access_key="mvm-secret://aws/secret-key",
    )

    string_to_sign = "\n".join(
        [
            "AWS4-HMAC-SHA256",
            "20130524T000000Z",
            "20130524/us-east-1/s3/aws4_request",
            hashlib.sha256(b"GET\n/\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD").hexdigest(),
        ]
    )
    signature = _sigv4_signature(
        creds.secret_access_key,
        "20130524",
        "us-east-1",
        "s3",
        string_to_sign,
    )

    assert creds.access_key_id == "AKIAIOSFODNN7EXAMPLE"
    assert mvm.constant_time_eq(
        signature,
        _sigv4_signature(
            placeholders["mvm-secret://aws/secret-key"],
            "20130524",
            "us-east-1",
            "s3",
            string_to_sign,
        ),
    )
    assert signature != _sigv4_signature(
        "mvm-secret://aws/secret-key",
        "20130524",
        "us-east-1",
        "s3",
        string_to_sign,
    )


def test_missing_handler_fails_closed():
    mvm.clear_substitution_handlers()
    try:
        mvm.aws_credentials_from_placeholders(
            access_key_id="mvm-secret://aws/access-key",
            secret_access_key="mvm-secret://aws/secret-key",
        )
    except mvm.SubstitutionHandlerError as exc:
        assert "aws" in str(exc)
    else:
        raise AssertionError("missing AWS substitution handler should fail closed")
