/*****************************************************************************
 *   Ledger App Bitcoin.
 *   (c) 2021 Ledger SAS.
 *
 *  Licensed under the Apache License, Version 2.0 (the "License");
 *  you may not use this file except in compliance with the License.
 *  You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 *  Unless required by applicable law or agreed to in writing, software
 *  distributed under the License is distributed on an "AS IS" BASIS,
 *  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  See the License for the specific language governing permissions and
 *  limitations under the License.
 *****************************************************************************/

#include <stdint.h>

#include "boilerplate/io.h"
#include "boilerplate/dispatcher.h"
#include "boilerplate/sw.h"
#include "../common/bip32.h"
#include "../commands.h"
#include "../constants.h"
#include "../crypto.h"
#include "../ui/display.h"
#include "../ui/menu.h"

#define H 0x80000000ul

static bool is_path_safe_for_pubkey_export(const uint32_t bip32_path[],
                                           size_t bip32_path_len,
                                           const uint32_t coin_types[],
                                           size_t coin_types_length) {
    // Exception for Qtum Electrum: it historically used "m/44h/88h/4541509h/1112098098h"
    // to derive encryption keys, so we whitelist it.
    if (bip32_path_len == 4 && bip32_path[0] == (44 ^ H) && bip32_path[1] == (88 ^ H) &&
        bip32_path[2] == (4541509 ^ H) && bip32_path[3] == (1112098098 ^ H)) {
        return true;
    } else if (bip32_path_len == 2 && bip32_path[0] == (0 ^ H) && bip32_path[1] == (45342 ^ H)) {
        // Exception for "m/0h/45342h"
        return true;
    } else if (bip32_path_len == 3 && bip32_path[0] == (20698 ^ H) && bip32_path[1] == (3053 ^ H) &&
               bip32_path[2] == (12648430 ^ H)) {
        // Exception for "m/20698h/3053h/12648430h"
        return true;
    }

    if (bip32_path_len < 3) {
        return false;
    }
    uint32_t purpose = bip32_path[0] & 0x7FFFFFFF;

    // most standard paths use 3 hardened derivation steps, but bip48 uses 4.
    size_t hardened_der_len;
    switch (purpose) {
        case 44:
        case 49:
        case 84:
        case 86:
            hardened_der_len = 3;
            break;
        case 45:
            // BIP-45 prescribes simply length 1, but we instead support existing deployed
            // use cases with path "m/45'/coin_type'/account'
            hardened_der_len = 3;
            break;
        case 48:
            hardened_der_len = 4;
            break;
        default:
            return false;
    }

    // bip32_path_len should be at least the hardened_der_len
    // (but it could have additional unhardened derivation steps)
    if (bip32_path_len < hardened_der_len) {
        return false;
    }

    for (unsigned int i = 0; i < hardened_der_len; i++) {
        if (bip32_path[i] < 0x80000000) {
            return false;
        }
    }
    // extra steps should not be hardened
    for (unsigned int i = hardened_der_len; i < bip32_path_len; i++) {
        if (bip32_path[i] >= 0x80000000) {
            return false;
        }
    }

    uint32_t coin_type = bip32_path[1] & 0x7FFFFFFF;
    bool coin_type_found = false;
    for (unsigned int i = 0; i < coin_types_length; i++) {
        if (coin_type == coin_types[i]) {
            coin_type_found = true;
        }
    }

    if (!coin_type_found) {
        return false;
    }

    uint32_t account = bip32_path[2] & 0x7FFFFFFF;

    // Account shouldn't be too large
    if (account > MAX_BIP44_ACCOUNT_RECOMMENDED) {
        return false;
    }

    // For BIP48, there is also the script type, with only standardized values 1' and 2'
    if (purpose == 48) {
        uint32_t script_type = bip32_path[3] & 0x7FFFFFFF;
        if (script_type != 1 && script_type != 2) {
            return false;
        }
    }

    return true;
}

void handler_get_extended_pubkey(dispatcher_context_t *dc, uint8_t protocol_version) {
    (void) protocol_version;

    LOG_PROCESSOR(__FILE__, __LINE__, __func__);

    // Device must be unlocked
    if (os_global_pin_is_validated() != BOLOS_UX_OK) {
        SEND_SW(dc, SW_SECURITY_STATUS_NOT_SATISFIED);
        return;
    }

    uint8_t display;
    uint8_t bip32_path_len;
    if (!buffer_read_u8(&dc->read_buffer, &display) ||
        !buffer_read_u8(&dc->read_buffer, &bip32_path_len)) {
        SEND_SW(dc, SW_WRONG_DATA_LENGTH);
        return;
    }

    if (display > 1 || bip32_path_len > MAX_BIP32_PATH_STEPS) {
        SEND_SW(dc, SW_INCORRECT_DATA);
        return;
    }

    uint32_t bip32_path[MAX_BIP32_PATH_STEPS];
    if (!buffer_read_bip32_path(&dc->read_buffer, bip32_path, bip32_path_len)) {
        SEND_SW(dc, SW_WRONG_DATA_LENGTH);
        return;
    }

    uint32_t coin_types[2] = {BIP44_COIN_TYPE, BIP44_COIN_TYPE_2};
    bool is_safe = is_path_safe_for_pubkey_export(bip32_path, bip32_path_len, coin_types, 2);

    if (!is_safe && !display) {
        SEND_SW(dc, SW_NOT_SUPPORTED);
        return;
    }

    char serialized_pubkey_str[MAX_SERIALIZED_PUBKEY_LENGTH + 1];

    int serialized_pubkey_len = get_serialized_extended_pubkey_at_path(bip32_path,
                                                                       bip32_path_len,
                                                                       BIP32_PUBKEY_VERSION,
                                                                       serialized_pubkey_str,
                                                                       NULL);
    if (serialized_pubkey_len == -1) {
        SEND_SW(dc, SW_BAD_STATE);
        return;
    }

    char path_str[MAX_SERIALIZED_BIP32_PATH_LENGTH + 1] = "(Master key)";
    if (bip32_path_len > 0) {
        bip32_path_format(bip32_path, bip32_path_len, path_str, sizeof(path_str));
    }

    if (display && !ui_display_pubkey(dc, path_str, !is_safe, serialized_pubkey_str)) {
        SEND_SW(dc, SW_DENY);
        return;
    }

    SEND_RESPONSE(dc, serialized_pubkey_str, strlen(serialized_pubkey_str), SW_OK);
}
