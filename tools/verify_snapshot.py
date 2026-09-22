#!/usr/bin/env python3
"""Validate saved public research evidence. No network access or transactions.

External layout, addresses, and serialization sources: docs/verification.md
and docs/verification/manifest.json. This is not a production account decoder.
"""

import base64
import hashlib
import json
from pathlib import Path


EVIDENCE = Path(__file__).resolve().parents[1] / "docs" / "verification"
# Alphabet and byte ordering verified from the base58 implementation linked
# in the source manifest. Used only to display IDL pubkey fields.
BASE58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def base58_encode(raw):
    value = int.from_bytes(raw, "big")
    encoded = ""
    while value:
        value, digit = divmod(value, len(BASE58))
        encoded = BASE58[digit] + encoded
    leading_zeroes = len(raw) - len(raw.lstrip(b"\0"))
    return BASE58[0] * leading_zeroes + encoded


class SnapshotDecoder:
    """Decode only the verified fixed-size field types in this Global IDL."""

    def __init__(self, data, offset):
        self.data = data
        self.offset = offset

    def read(self, field_type):
        if isinstance(field_type, dict):
            require(set(field_type) == {"array"}, "Unsupported IDL field type")
            subtype, count = field_type["array"]
            return [self.read(subtype) for _ in range(count)]
        sizes = {"bool": 1, "u8": 1, "u64": 8, "pubkey": 32}
        require(field_type in sizes, f"Unsupported IDL type: {field_type}")
        size = sizes[field_type]
        raw = self.data[self.offset:self.offset + size]
        require(len(raw) == size, "Truncated account")
        self.offset += size
        if field_type == "pubkey":
            return base58_encode(raw)
        if field_type == "bool":
            require(raw[0] in (0, 1), "Invalid Borsh boolean")
            return bool(raw[0])
        return int.from_bytes(raw, "little")


def rpc_account(response):
    require("error" not in response, "Saved RPC returned an error")
    result = response["result"]
    require(result["value"] is not None, "Account absent in saved RPC")
    require(not result["value"]["executable"], "Unexpected executable account")
    return result["context"]["slot"], result["value"]


def units(raw, decimals):
    whole, remainder = divmod(raw, 10 ** decimals)
    return f"{whole:,}.{remainder:0{decimals}d}"


def main():
    manifest = json.loads((EVIDENCE / "manifest.json").read_text())
    documents = {}
    for name, source in manifest["files"].items():
        raw = (EVIDENCE / name).read_bytes()
        require(hashlib.sha256(raw).hexdigest() == source["sha256"],
                f"Snapshot hash mismatch: {name}")
        documents[name] = json.loads(raw)

    idl = documents["pump-idl.json"]
    require(idl["address"] == manifest["program_address"], "Program ID mismatch")
    global_slot, account = rpc_account(documents["pump-global.rpc.json"])
    require(account["owner"] == idl["address"], "Global owner mismatch")
    encoded, encoding = account["data"]
    require(encoding == "base64", "Unexpected Global encoding")
    data = base64.b64decode(encoded, validate=True)
    discriminator = bytes(next(a for a in idl["accounts"]
                               if a["name"] == "Global")["discriminator"])
    require(data[:len(discriminator)] == discriminator, "Discriminator mismatch")
    fields = next(t for t in idl["types"]
                  if t["name"] == "Global")["type"]["fields"]
    decoder = SnapshotDecoder(data, len(discriminator))
    global_state = {f["name"]: decoder.read(f["type"]) for f in fields}
    require(decoder.offset == len(data) == account["space"],
            "Snapshot and IDL lengths differ; re-verify the layout")
    require(manifest["quote_mint"] in global_state["whitelisted_quote_mints"],
            "USDC absent from Global quote whitelist")

    mint_slot, mint = rpc_account(documents["usdc-mint.rpc.json"])
    # USDC is a legacy SPL mint; its owner is recorded directly in the public
    # snapshot. It is checked against the IDL's legacy create instruction.
    legacy_accounts = next(i for i in idl["instructions"]
                           if i["name"] == "create")["accounts"]
    legacy_program = next(a for a in legacy_accounts
                          if a["name"] == "token_program")["address"]
    require(mint["owner"] == legacy_program, "USDC mint owner mismatch")
    require(mint["data"]["program"] == "spl-token", "Unexpected mint parser")
    parsed = mint["data"]["parsed"]
    require(parsed["type"] == "mint", "Expected parsed USDC mint")
    require(parsed["info"]["isInitialized"], "USDC mint is uninitialized")
    quote_decimals = parsed["info"]["decimals"]
    require(quote_decimals == 6, "USDC precision differs from verified evidence")

    x = global_state["initial_virtual_token_reserves"]
    y = global_state["initial_virtual_quote_reserves"]
    sellable = global_state["initial_real_token_reserves"]
    supply = global_state["token_total_supply"]
    require(0 < sellable < x and sellable <= supply and y > 0,
            "Invalid initial reserves")
    # Pre-fee exact-output arithmetic read from official SDK 2.0.0:
    # src/bondingCurve.ts:getBuySolAmountFromTokenAmountQuote.
    # This does not calculate the live fee schedule or simulate the program.
    net_quote = sellable * y // (x - sellable) + 1
    token_decimals = manifest["base_token_decimals"]
    output = {
        "scope": "saved mainnet snapshots; not a live quote or execution test",
        "hashes_owner_discriminator_and_layout": "verified",
        "global_slot": global_slot,
        "global_account_bytes": len(data),
        "usdc_mint_slot": mint_slot,
        "usdc_allowlisted": True,
        "virtual_tokens": units(x, token_decimals),
        "virtual_usdc": units(y, quote_decimals),
        "total_tokens": units(supply, token_decimals),
        "sellable_tokens": units(sellable, token_decimals),
        "remaining_supply_tokens": units(supply - sellable, token_decimals),
        "single_buy_net_usdc_to_sell_out": units(net_quote, quote_decimals),
        "fees_conversion_rent_and_distribution_costs": "not calculated",
    }
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
