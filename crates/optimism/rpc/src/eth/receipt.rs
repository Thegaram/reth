//! Loads and formats OP receipt RPC response.   

use op_alloy_rpc_types::{receipt::L1BlockInfo, OptimismTransactionReceiptFields};
use reth_chainspec::{ChainSpec, OptimismHardforks};
use reth_evm_optimism::RethL1BlockInfo;
use reth_node_api::{FullNodeComponents, NodeCore};
use reth_primitives::{Receipt, TransactionMeta, TransactionSigned};
use reth_provider::ChainSpecProvider;
use reth_rpc_eth_api::{
    helpers::{EthApiSpec, LoadReceipt, LoadTransaction},
    FromEthApiError,
};
use reth_rpc_eth_types::{EthApiError, EthStateCache, ReceiptBuilder};
use reth_rpc_types::AnyTransactionReceipt;

use crate::{OpEthApi, OpEthApiError};

impl<N> LoadReceipt for OpEthApi<N>
where
    Self: EthApiSpec + LoadTransaction<Error = OpEthApiError>,
    N: FullNodeComponents,
{
    #[inline]
    fn cache(&self) -> &EthStateCache {
        self.inner.cache()
    }

    async fn build_transaction_receipt(
        &self,
        tx: TransactionSigned,
        meta: TransactionMeta,
        receipt: Receipt,
    ) -> Result<AnyTransactionReceipt, Self::Error> {
        let (block, receipts) = LoadReceipt::cache(self)
            .get_block_and_receipts(meta.block_hash)
            .await
            .map_err(Self::Error::from_eth_err)?
            .ok_or(Self::Error::from_eth_err(EthApiError::UnknownBlockNumber))?;

        let block = block.unseal();
        let l1_block_info =
            reth_evm_optimism::extract_l1_info(&block).map_err(OpEthApiError::from)?;

        let op_receipt_meta = self
            .build_op_receipt_meta(&tx, l1_block_info, &receipt)
            .map_err(OpEthApiError::from)?;

        let receipt_resp = ReceiptBuilder::new(&tx, meta, &receipt, &receipts)
            .map_err(Self::Error::from_eth_err)?
            .add_other_fields(op_receipt_meta.into())
            .build();

        Ok(receipt_resp)
    }
}

impl<N> OpEthApi<N>
where
    N: NodeCore<Provider: ChainSpecProvider<ChainSpec = ChainSpec>>,
{
    /// Builds a receipt w.r.t. chain spec.
    pub fn build_op_receipt_meta(
        &self,
        tx: &TransactionSigned,
        l1_block_info: revm::L1BlockInfo,
        receipt: &Receipt,
    ) -> Result<OptimismTransactionReceiptFields, OpEthApiError> {
        Ok(OpReceiptFieldsBuilder::default()
            .l1_block_info(&self.inner.provider().chain_spec(), tx, l1_block_info)?
            .deposit_nonce(receipt.deposit_nonce)
            .deposit_version(receipt.deposit_receipt_version)
            .build())
    }
}

/// L1 fee and data gas for a non-deposit transaction, or deposit nonce and receipt version for a
/// deposit transaction.
#[derive(Debug, Default, Clone)]
pub struct OpReceiptFieldsBuilder {
    /// Block timestamp.
    pub l1_block_timestamp: u64,
    /// The L1 fee for transaction.
    pub l1_fee: Option<u128>,
    /// L1 gas used by transaction.
    pub l1_data_gas: Option<u128>,
    /// L1 fee scalar.
    pub l1_fee_scalar: Option<f64>,
    /* ---------------------------------------- Bedrock ---------------------------------------- */
    /// The base fee of the L1 origin block.
    pub l1_base_fee: Option<u128>,
    /* --------------------------------------- Regolith ---------------------------------------- */
    /// Deposit nonce, if this is a deposit transaction.
    pub deposit_nonce: Option<u64>,
    /* ---------------------------------------- Canyon ----------------------------------------- */
    /// Deposit receipt version, if this is a deposit transaction.
    pub deposit_receipt_version: Option<u64>,
    /* ---------------------------------------- Ecotone ---------------------------------------- */
    /// The current L1 fee scalar.
    pub l1_base_fee_scalar: Option<u128>,
    /// The current L1 blob base fee.
    pub l1_blob_base_fee: Option<u128>,
    /// The current L1 blob base fee scalar.
    pub l1_blob_base_fee_scalar: Option<u128>,
}

impl OpReceiptFieldsBuilder {
    /// Returns a new builder.
    pub fn new(block_timestamp: u64) -> Self {
        Self { l1_block_timestamp: block_timestamp, ..Default::default() }
    }

    /// Applies [`L1BlockInfo`](revm::L1BlockInfo).
    pub fn l1_block_info(
        mut self,
        chain_spec: &ChainSpec,
        tx: &TransactionSigned,
        l1_block_info: revm::L1BlockInfo,
    ) -> Result<Self, OpEthApiError> {
        let envelope_buf = tx.envelope_encoded();
        let timestamp = self.l1_block_timestamp;

        self.l1_fee = Some(
            l1_block_info
                .l1_tx_data_fee(chain_spec, timestamp, &tx.envelope_encoded(), tx.is_deposit())
                .map_err(|_| OpEthApiError::L1BlockFeeError)?
                .saturating_to(),
        );

        self.l1_data_gas = Some(
            l1_block_info
                .l1_data_gas(chain_spec, timestamp, &envelope_buf)
                .map_err(|_| OpEthApiError::L1BlockGasError)?
                .saturating_add(l1_block_info.l1_fee_overhead.unwrap_or_default())
                .saturating_to(),
        );

        self.l1_fee_scalar = (!chain_spec.hardforks.is_ecotone_active_at_timestamp(timestamp))
            .then_some(f64::from(l1_block_info.l1_base_fee_scalar) / 1_000_000.0);

        self.l1_base_fee = Some(l1_block_info.l1_base_fee.saturating_to());
        self.l1_base_fee_scalar = Some(l1_block_info.l1_base_fee_scalar.saturating_to());
        self.l1_blob_base_fee = l1_block_info.l1_blob_base_fee.map(|fee| fee.saturating_to());
        self.l1_blob_base_fee_scalar =
            l1_block_info.l1_blob_base_fee_scalar.map(|scalar| scalar.saturating_to());

        Ok(self)
    }

    /// Applies deposit transaction metadata: deposit nonce.
    pub const fn deposit_nonce(mut self, nonce: Option<u64>) -> Self {
        self.deposit_nonce = nonce;
        self
    }

    /// Applies deposit transaction metadata: deposit receipt version.
    pub const fn deposit_version(mut self, version: Option<u64>) -> Self {
        self.deposit_receipt_version = version;
        self
    }

    /// Builds the [`OptimismTransactionReceiptFields`] object.
    pub const fn build(self) -> OptimismTransactionReceiptFields {
        let Self {
            l1_block_timestamp: _, // used to compute other fields
            l1_fee,
            l1_data_gas: l1_gas_used,
            l1_fee_scalar,
            l1_base_fee: l1_gas_price,
            deposit_nonce,
            deposit_receipt_version,
            l1_base_fee_scalar,
            l1_blob_base_fee,
            l1_blob_base_fee_scalar,
        } = self;

        OptimismTransactionReceiptFields {
            l1_block_info: L1BlockInfo {
                l1_gas_price,
                l1_gas_used,
                l1_fee,
                l1_fee_scalar,
                l1_base_fee_scalar,
                l1_blob_base_fee,
                l1_blob_base_fee_scalar,
            },
            deposit_nonce,
            deposit_receipt_version,
        }
    }
}

#[cfg(test)]
mod test {
    use reth_optimism_chainspec::OP_MAINNET;
    use reth_primitives::Block;

    use super::*;

    /// OP Mainnet transaction at index 0 in block 124665056.
    ///
    /// <https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a>
    const TX_SET_L1_BLOCK_OP_MAINNET_BLOCK_124665056: &str = r#"{"hash":"0x312e290cf36df704a2217b015d6455396830b0ce678b860ebfcc30f41403d7b1","signature":{"r":0,"s":0,"odd_y_parity":false},"transaction":{"Legacy":{"chain_id":10,"nonce":19429994,"gas_price":142541,"gas_limit":300000,"to":"0x4200000000000000000000000000000000000015","value":0,"input":"0x440a5e200000146b000f79c500000000000000040000000066d052e700000000013ad8a3000000000000000000000000000000000000000000000000000000003ef1278700000000000000000000000000000000000000000000000000000000000000012fdf87b89884a61e74b322bbcf60386f543bfae7827725efaaf0ab1de2294a590000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"}}}"#;

    /// OP Mainnet transaction at index 1 in block 124665056.
    ///
    /// <https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a>
    const TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056: &str = r#"{"hash":"0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a","signature":{"r":"13007002095092827311851034925236438545740246347271474487442111518983474121409","s":"17538143614307881621478789077517273085472409472482011425440968747716890363014","odd_y_parity":true},"transaction":{"Legacy":{"chain_id":10,"nonce":261031,"gas_price":142541,"gas_limit":300000,"to":"0x3E6f4f7866654c18f536170780344AA8772950b6","value":0,"input":"0x6a761202000000000000000000000000087000a300de7200382b55d40045000000e5d60e0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003a0000000000000000000000000000000000000000000000000000000000000022482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000120000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f1000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000044095ea7b300000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c0000000000000000000000000000000000000000000000049b9ca9a6943400000000000000000000000000000000000000000000000000000000000000000000000000000000000021c4928109acb0659a88ae5329b5374a3024694c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000024b6b55f250000000000000000000000000000000000000000000000049b9ca9a694340000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000415ec214a3950bea839a7e6fbb0ba1540ac2076acd50820e2d5ef83d0902cdffb24a47aff7de5190290769c4f0a9c6fabf63012986a0d590b1b571547a8c7050ea1b00000000000000000000000000000000000000000000000000000000000000"}}}"#;

    /// L1 block info for transaction at index 1 in block 124665056.
    ///
    /// <https://optimistic.etherscan.io/tx/0x1059e8004daff32caa1f1b1ef97fe3a07a8cf40508f5b835b66d9420d87c4a4a>
    const L1_META_TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056: OptimismTransactionReceiptFields =
        OptimismTransactionReceiptFields {
            l1_block_info: L1BlockInfo {
                l1_gas_price: Some(1055991687), // since bedrock l1 base fee
                l1_gas_used: Some(4471),
                l1_fee: Some(24681034813),
                l1_fee_scalar: None,
                l1_base_fee_scalar: Some(0), /* todo: what was it for tx 0x10..41 in block
                                              * mainnet
                                              * 124665056? */
                l1_blob_base_fee: None,
                l1_blob_base_fee_scalar: None,
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        };

    #[test]
    fn op_receipt_fields_from_block_and_tx() {
        // rig
        let tx_0 =
            serde_json::from_str::<TransactionSigned>(TX_SET_L1_BLOCK_OP_MAINNET_BLOCK_124665056)
                .unwrap();

        let tx_1 =
            serde_json::from_str::<TransactionSigned>(TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056)
                .unwrap();

        let block = Block { body: [tx_0, tx_1.clone()].to_vec(), ..Default::default() };

        let l1_block_info =
            reth_evm_optimism::extract_l1_info(&block).expect("should extract l1 info");

        // test
        let receipt_meta = OpReceiptFieldsBuilder::new(1724928899)
            .l1_block_info(&OP_MAINNET, &tx_1, l1_block_info)
            .expect("should parse revm l1 info")
            .build();

        let L1BlockInfo {
            l1_gas_price,
            l1_gas_used,
            l1_fee,
            l1_fee_scalar,
            l1_base_fee_scalar,
            ..
        } = receipt_meta.l1_block_info;

        assert_eq!(
            l1_gas_price,
            L1_META_TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_gas_price,
            "incorrect l1 base fee (former gas price)"
        );
        assert_eq!(
            l1_gas_used, L1_META_TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_gas_used,
            "incorrect l1 gas used"
        );
        assert_eq!(
            l1_fee, L1_META_TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_fee,
            "incorrect l1 fee"
        );
        assert_eq!(
            l1_fee_scalar,
            L1_META_TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_fee_scalar,
            "incorrect l1 fee scalar"
        );
        assert_eq!(
            l1_base_fee_scalar,
            L1_META_TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056.l1_block_info.l1_base_fee_scalar,
            "incorrect l1 base fee scalar"
        );

        // can catch some bugs from failing to upgrade for next hf
        assert_eq!(receipt_meta, L1_META_TX_1_EXEC_TX_OP_MAINNET_BLOCK_124665056);
    }
}
