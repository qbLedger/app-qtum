use core::fmt::Debug;
use core::str::FromStr;

use async_trait::async_trait;

use bitcoin::{
    consensus::encode::{deserialize_partial, VarInt},
    secp256k1::ecdsa::Signature,
    util::{
        bip32::{DerivationPath, ExtendedPubKey, Fingerprint},
        psbt::PartiallySignedTransaction as Psbt,
    },
};

#[cfg(feature = "paranoid_client")]
use miniscript::{Descriptor, DescriptorPublicKey};

use crate::{
    apdu::{APDUCommand, StatusWord},
    command,
    error::BitcoinClientError,
    interpreter::{get_merkleized_map_commitment, ClientCommandInterpreter},
    psbt::*,
    wallet::WalletPolicy,
};

/// BitcoinClient calls and interprets commands with the Ledger Device.
/// The methods can only be used by an asynchronous engine like tokio.
pub struct BitcoinClient<T: Transport> {
    transport: T,
}

impl<T: Transport> BitcoinClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    async fn make_request(
        &self,
        req: &APDUCommand,
        interpreter: Option<&mut ClientCommandInterpreter>,
    ) -> Result<Vec<u8>, BitcoinClientError<T::Error>> {
        let (mut sw, mut data) = self
            .transport
            .exchange(req)
            .await
            .map_err(BitcoinClientError::Transport)?;

        if let Some(interpreter) = interpreter {
            while sw == StatusWord::InterruptedExecution {
                let response = interpreter.execute(data)?;
                let res = self
                    .transport
                    .exchange(&command::continue_interrupted(response))
                    .await
                    .map_err(BitcoinClientError::Transport)?;
                sw = res.0;
                data = res.1;
            }
        }

        if sw != StatusWord::OK {
            Err(BitcoinClientError::Device {
                status: sw,
                command: req.ins,
            })
        } else {
            Ok(data)
        }
    }

    // Verifies that the address that the application returns matches the one independently
    // computed on the client
    #[cfg(feature = "paranoid_client")]
    async fn check_address(
        &self,
        wallet: &WalletPolicy,
        change: bool,
        address_index: u32,
        expected_address: &bitcoin::Address,
    ) -> Result<(), BitcoinClientError<T::Error>> {
        let desc_str = wallet
            .get_descriptor(change)
            .map_err(|_| BitcoinClientError::ClientError("Failed to get descriptor".to_string()))?;
        let descriptor = Descriptor::<DescriptorPublicKey>::from_str(&desc_str).map_err(|_| {
            BitcoinClientError::ClientError("Failed to parse descriptor".to_string())
        })?;

        if descriptor
            .at_derivation_index(address_index)
            .script_pubkey()
            != expected_address.script_pubkey()
        {
            return Err(BitcoinClientError::InvalidResponse("Invalid address. Please update your Bitcoin app. If the problem persists, report a bug at https://github.com/LedgerHQ/app-bitcoin-new".to_string()));
        }

        Ok(())
    }

    /// Returns the currently running app's name, version and state flags
    pub async fn get_version(
        &self,
    ) -> Result<(String, String, Vec<u8>), BitcoinClientError<T::Error>> {
        let cmd = command::get_version();
        let data = self.make_request(&cmd, None).await?;
        if data.is_empty() || data[0] != 0x01 {
            return Err(BitcoinClientError::UnexpectedResult {
                command: cmd.ins,
                data,
            });
        }

        let (name, i): (String, usize) =
            deserialize_partial(&data[1..]).map_err(|_| BitcoinClientError::UnexpectedResult {
                command: cmd.ins,
                data: data.clone(),
            })?;

        let (version, j): (String, usize) = deserialize_partial(&data[i + 1..]).map_err(|_| {
            BitcoinClientError::UnexpectedResult {
                command: cmd.ins,
                data: data.clone(),
            }
        })?;

        let (flags, _): (Vec<u8>, usize) =
            deserialize_partial(&data[i + j + 1..]).map_err(|_| {
                BitcoinClientError::UnexpectedResult {
                    command: cmd.ins,
                    data: data.clone(),
                }
            })?;

        Ok((name, version, flags))
    }

    /// Retrieve the master fingerprint.
    pub async fn get_master_fingerprint(
        &self,
    ) -> Result<Fingerprint, BitcoinClientError<T::Error>> {
        let cmd = command::get_master_fingerprint();
        self.make_request(&cmd, None)
            .await
            .map(|data| Fingerprint::from(data.as_slice()))
    }

    /// Retrieve the bip32 extended pubkey derived with the given path
    /// and optionally display it on screen
    pub async fn get_extended_pubkey(
        &self,
        path: &DerivationPath,
        display: bool,
    ) -> Result<ExtendedPubKey, BitcoinClientError<T::Error>> {
        let cmd = command::get_extended_pubkey(path, display);
        self.make_request(&cmd, None).await.and_then(|data| {
            ExtendedPubKey::from_str(&String::from_utf8_lossy(&data)).map_err(|_| {
                BitcoinClientError::UnexpectedResult {
                    command: cmd.ins,
                    data,
                }
            })
        })
    }

    /// Registers the given wallet policy, returns the wallet ID and HMAC.
    pub async fn register_wallet(
        &self,
        wallet: &WalletPolicy,
    ) -> Result<([u8; 32], [u8; 32]), BitcoinClientError<T::Error>> {
        let cmd = command::register_wallet(wallet);
        let mut intpr = ClientCommandInterpreter::new();
        intpr.add_known_preimage(wallet.serialize());
        let keys: Vec<String> = wallet.keys.iter().map(|k| k.to_string()).collect();
        intpr.add_known_list(&keys);
        //necessary for version 1 of the protocol (introduced in version 2.1.0)
        intpr.add_known_preimage(wallet.descriptor_template.as_bytes().to_vec());
        let (id, hmac) = self
            .make_request(&cmd, Some(&mut intpr))
            .await
            .and_then(|data| {
                if data.len() < 64 {
                    Err(BitcoinClientError::UnexpectedResult {
                        command: cmd.ins,
                        data,
                    })
                } else {
                    let mut id = [0x00; 32];
                    id.copy_from_slice(&data[0..32]);
                    let mut hmac = [0x00; 32];
                    hmac.copy_from_slice(&data[32..64]);
                    Ok((id, hmac))
                }
            })?;

        #[cfg(feature = "paranoid_client")]
        {
            let device_addr = self
                .get_wallet_address(wallet, Some(&hmac), false, 0, false)
                .await?;

            self.check_address(wallet, false, 0, &device_addr).await?;
        }

        Ok((id, hmac))
    }

    /// For a given wallet that was already registered on the device (or a standard wallet that does not need registration),
    /// returns the address for a certain `change`/`address_index` combination.
    pub async fn get_wallet_address(
        &self,
        wallet: &WalletPolicy,
        wallet_hmac: Option<&[u8; 32]>,
        change: bool,
        address_index: u32,
        display: bool,
    ) -> Result<bitcoin::Address, BitcoinClientError<T::Error>> {
        let mut intpr = ClientCommandInterpreter::new();
        intpr.add_known_preimage(wallet.serialize());
        let keys: Vec<String> = wallet.keys.iter().map(|k| k.to_string()).collect();
        intpr.add_known_list(&keys);
        // necessary for version 1 of the protocol (introduced in version 2.1.0)
        intpr.add_known_preimage(wallet.descriptor_template.as_bytes().to_vec());
        let cmd = command::get_wallet_address(wallet, wallet_hmac, change, address_index, display);
        let address = self
            .make_request(&cmd, Some(&mut intpr))
            .await
            .and_then(|data| {
                bitcoin::Address::from_str(&String::from_utf8_lossy(&data)).map_err(|_| {
                    BitcoinClientError::UnexpectedResult {
                        command: cmd.ins,
                        data,
                    }
                })
            })?;

        #[cfg(feature = "paranoid_client")]
        {
            self.check_address(wallet, change, address_index, &address)
                .await?;
        }

        Ok(address)
    }

    /// Signs a PSBT using a registered wallet (or a standard wallet that does not need registration).
    /// Signature requires explicit approval from the user.
    #[allow(clippy::type_complexity)]
    pub async fn sign_psbt(
        &self,
        psbt: &Psbt,
        wallet: &WalletPolicy,
        wallet_hmac: Option<&[u8; 32]>,
    ) -> Result<Vec<(usize, PartialSignature)>, BitcoinClientError<T::Error>> {
        let mut intpr = ClientCommandInterpreter::new();
        intpr.add_known_preimage(wallet.serialize());
        let keys: Vec<String> = wallet.keys.iter().map(|k| k.to_string()).collect();
        intpr.add_known_list(&keys);
        // necessary for version 1 of the protocol (introduced in version 2.1.0)
        intpr.add_known_preimage(wallet.descriptor_template.as_bytes().to_vec());

        let global_map: Vec<(Vec<u8>, Vec<u8>)> = get_v2_global_pairs(psbt)
            .into_iter()
            .map(deserialize_pairs)
            .collect();
        intpr.add_known_mapping(&global_map);
        let global_mapping_commitment = get_merkleized_map_commitment(&global_map);

        let mut input_commitments: Vec<Vec<u8>> = Vec::with_capacity(psbt.inputs.len());
        for (index, input) in psbt.inputs.iter().enumerate() {
            let txin = psbt
                .unsigned_tx
                .input
                .get(index)
                .ok_or(BitcoinClientError::InvalidPsbt)?;
            let input_map: Vec<(Vec<u8>, Vec<u8>)> = get_v2_input_pairs(input, txin)
                .into_iter()
                .map(deserialize_pairs)
                .collect();
            intpr.add_known_mapping(&input_map);
            input_commitments.push(get_merkleized_map_commitment(&input_map));
        }
        let input_commitments_root = intpr.add_known_list(&input_commitments);

        let mut output_commitments: Vec<Vec<u8>> = Vec::with_capacity(psbt.outputs.len());
        for (index, output) in psbt.outputs.iter().enumerate() {
            let txout = psbt
                .unsigned_tx
                .output
                .get(index)
                .ok_or(BitcoinClientError::InvalidPsbt)?;
            let output_map: Vec<(Vec<u8>, Vec<u8>)> = get_v2_output_pairs(output, txout)
                .into_iter()
                .map(deserialize_pairs)
                .collect();
            intpr.add_known_mapping(&output_map);
            output_commitments.push(get_merkleized_map_commitment(&output_map));
        }
        let output_commitments_root = intpr.add_known_list(&output_commitments);

        let cmd = command::sign_psbt(
            &global_mapping_commitment,
            psbt.inputs.len(),
            &input_commitments_root,
            psbt.outputs.len(),
            &output_commitments_root,
            wallet,
            wallet_hmac,
        );

        self.make_request(&cmd, Some(&mut intpr)).await?;

        let results = intpr.yielded();
        if results.iter().any(|res| res.len() <= 1) {
            return Err(BitcoinClientError::UnexpectedResult {
                command: cmd.ins,
                data: results.into_iter().fold(Vec::new(), |mut acc, res| {
                    acc.extend(res);
                    acc
                }),
            });
        }

        let mut signatures = Vec::new();
        for result in results {
            let (input_index, i): (VarInt, usize) =
                deserialize_partial(&result).map_err(|_| BitcoinClientError::UnexpectedResult {
                    command: cmd.ins,
                    data: result.clone(),
                })?;

            signatures.push((
                input_index.0 as usize,
                PartialSignature::from_slice(&result[i..]).map_err(|_| {
                    BitcoinClientError::UnexpectedResult {
                        command: cmd.ins,
                        data: result.clone(),
                    }
                })?,
            ));
        }

        Ok(signatures)
    }

    /// Sign a message with the key derived with the given derivation path.
    /// Result is the header byte (31-34: P2PKH compressed) and the ecdsa signature.
    pub async fn sign_message(
        &self,
        message: &[u8],
        path: &DerivationPath,
    ) -> Result<(u8, Signature), BitcoinClientError<T::Error>> {
        let chunks: Vec<&[u8]> = message.chunks(64).collect();
        let mut intpr = ClientCommandInterpreter::new();
        let message_commitment_root = intpr.add_known_list(&chunks);
        let cmd = command::sign_message(message.len(), &message_commitment_root, path);
        self.make_request(&cmd, Some(&mut intpr))
            .await
            .and_then(|data| {
                Ok((
                    data[0],
                    Signature::from_compact(&data[1..]).map_err(|_| {
                        BitcoinClientError::UnexpectedResult {
                            command: cmd.ins,
                            data: data.to_vec(),
                        }
                    })?,
                ))
            })
    }
}

/// Asynchronous communication layer between the bitcoin client and the Ledger device.
#[async_trait]
pub trait Transport {
    type Error: Debug;
    async fn exchange(&self, command: &APDUCommand) -> Result<(StatusWord, Vec<u8>), Self::Error>;
}
