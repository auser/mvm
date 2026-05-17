import { createHash, createHmac } from "node:crypto";
import { describe, expect, it, afterEach } from "vitest";
import * as mvm from "../src/index.js";

function sign(key: Buffer | string, msg: string): Buffer {
  return createHmac("sha256", key).update(msg, "utf8").digest();
}

function sigv4Signature(
  secret: string,
  date: string,
  region: string,
  service: string,
  stringToSign: string,
): string {
  const dateKey = sign(`AWS4${secret}`, date);
  const regionKey = sign(dateKey, region);
  const serviceKey = sign(regionKey, service);
  const signingKey = sign(serviceKey, "aws4_request");
  return createHmac("sha256", signingKey).update(stringToSign, "utf8").digest("hex");
}

afterEach(() => {
  mvm.clear_substitution_handlers();
});

describe("runtime substitution handlers", () => {
  it("resolves AWS credentials before SigV4 signing", async () => {
    const placeholders = new Map([
      ["mvm-secret://aws/access-key", "AKIAIOSFODNN7EXAMPLE"],
      ["mvm-secret://aws/secret-key", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"],
    ]);
    mvm.register_substitution_handler("aws", (placeholder) => {
      const value = placeholders.get(placeholder);
      if (value === undefined) {
        throw new Error(`unknown placeholder ${placeholder}`);
      }
      return value;
    });

    const creds = await mvm.aws_credentials_from_placeholders({
      accessKeyId: "mvm-secret://aws/access-key",
      secretAccessKey: "mvm-secret://aws/secret-key",
    });
    const canonical = "GET\n/\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD";
    const stringToSign = [
      "AWS4-HMAC-SHA256",
      "20130524T000000Z",
      "20130524/us-east-1/s3/aws4_request",
      createHash("sha256").update(canonical, "utf8").digest("hex"),
    ].join("\n");

    const signature = sigv4Signature(
      creds.secretAccessKey,
      "20130524",
      "us-east-1",
      "s3",
      stringToSign,
    );

    expect(creds.accessKeyId).toBe("AKIAIOSFODNN7EXAMPLE");
    expect(signature).toBe(
      sigv4Signature(
        "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        "20130524",
        "us-east-1",
        "s3",
        stringToSign,
      ),
    );
    expect(signature).not.toBe(
      sigv4Signature("mvm-secret://aws/secret-key", "20130524", "us-east-1", "s3", stringToSign),
    );
  });

  it("fails closed when no handler is registered", async () => {
    await expect(
      mvm.aws_credentials_from_placeholders({
        accessKeyId: "mvm-secret://aws/access-key",
        secretAccessKey: "mvm-secret://aws/secret-key",
      }),
    ).rejects.toThrow(mvm.SubstitutionHandlerError);
  });
});
