// LNP/BP Core Library implementing LNPBP specifications & standards
// Written in 2020 by
//     Dr. Maxim Orlovsky <orlovsky@pandoracore.com>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the MIT License
// along with this software.
// If not, see <https://opensource.org/licenses/MIT>.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use bitcoin::{Address, Network, Script};
use miniscript::descriptor::{ShInner, SortedMultiVec, WshInner};
use miniscript::{
    policy, Descriptor, DescriptorTrait, Error, Miniscript, MiniscriptKey,
    Satisfier, ScriptContext, ToPublicKey,
};
use strict_encoding::{StrictDecode, StrictEncode};

use super::{OuterCategory, OuterType, ParseError};

#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate", rename_all = "lowercase")
)]
#[derive(
    Clone,
    Copy,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
    Hash,
    Debug,
    Display,
    StrictEncode,
    StrictDecode,
)]
#[repr(u8)]
pub enum ContractType {
    #[display("singlesig")]
    SingleSig,

    #[display("multisig")]
    MultiSig,

    #[display("script")]
    Script,
}

impl FromStr for ContractType {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().trim() {
            "singlesig" => ContractType::SingleSig,
            "multisig" => ContractType::MultiSig,
            "script" => ContractType::Script,
            unknown => {
                return Err(ParseError::UnrecognizedDescriptorName(
                    unknown.to_owned(),
                ))
            }
        })
    }
}

/// NB: Sorted bare multisigs are not supported and will produce unsorted bare
/// multisig instead
#[derive(
    Clone,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
    Hash,
    Debug,
    StrictEncode,
    StrictDecode,
)]
pub enum ContractDescriptor<Pk>
where
    Pk: MiniscriptKey + FromStr + StrictEncode + StrictDecode,
    <Pk as FromStr>::Err: Display,
    <Pk as MiniscriptKey>::Hash: FromStr,
    <<Pk as MiniscriptKey>::Hash as FromStr>::Err: Display,
{
    SingleSig {
        category: OuterCategory,
        pk: Pk,
    },

    MultiSig {
        category: OuterCategory,
        threshold: usize,
        signers: Vec<Pk>,
        sorted: bool,
    },

    Script {
        // TODO: Do not participate cache in strict-encoded data
        ms_cache: CompiledMiniscript<Pk>, /* This is cache, policy is the
                                           * master source */
        policy: policy::Concrete<Pk>,
    },
}

impl<Pk> ContractDescriptor<Pk>
where
    Pk: MiniscriptKey + FromStr + StrictEncode + StrictDecode,
    <Pk as FromStr>::Err: Display,
    <Pk as MiniscriptKey>::Hash: FromStr,
    <<Pk as MiniscriptKey>::Hash as FromStr>::Err: Display,
{
    fn multisig_miniscript<Ctx: ScriptContext>(
        threshold: usize,
        signers: &Vec<Pk>,
    ) -> Miniscript<Pk, Ctx> {
        policy::Concrete::Threshold(
            threshold,
            signers
                .iter()
                .map(|pk| policy::Concrete::Key(pk.clone()))
                .collect(),
        )
        .compile::<Ctx>()
        .expect("Internal error in multisig miniscript policy composition")
    }

    pub fn with<Ctx: ScriptContext>(
        ms: Miniscript<Pk, Ctx>,
        category: OuterCategory,
        policy_source: &str,
    ) -> Result<ContractDescriptor<Pk>, miniscript::Error> {
        Ok(match ms.node {
            miniscript::Terminal::PkK(pk) => {
                ContractDescriptor::SingleSig { category, pk }
            }

            miniscript::Terminal::Multi(threshold, signers) => {
                ContractDescriptor::MultiSig {
                    category,
                    threshold,
                    signers,
                    sorted: false,
                }
            }

            _ => {
                let policy = policy::Concrete::from_str(policy_source)?;
                ContractDescriptor::Script {
                    ms_cache: CompiledMiniscript::with(&policy, category)?,
                    policy,
                }
            }
        })
    }

    pub fn to_descriptor(&self, nested: bool) -> Descriptor<Pk> {
        match self {
            ContractDescriptor::SingleSig {
                category: OuterCategory::Bare,
                pk,
            } => Descriptor::new_pk(pk.clone()),
            ContractDescriptor::SingleSig {
                category: OuterCategory::Hashed,
                pk,
            } => Descriptor::new_pkh(pk.clone()),
            ContractDescriptor::SingleSig {
                category: OuterCategory::SegWit,
                pk,
            } => {
                if nested {
                    Descriptor::new_sh_wpkh(pk.clone())
                        .expect("Internal scripting engine inconsistency")
                } else {
                    Descriptor::new_wpkh(pk.clone())
                        .expect("Internal scripting engine inconsistency")
                }
            }
            ContractDescriptor::SingleSig {
                category: OuterCategory::Taproot,
                ..
            } => panic!("Taproot not yet supported"),
            // TODO: Descriptor::new_tr(pk),
            ContractDescriptor::MultiSig {
                category: OuterCategory::Bare,
                threshold,
                signers,
                sorted: _, // TODO: Support sorded bare multisigs
            } => Descriptor::new_bare(ContractDescriptor::multisig_miniscript(
                *threshold, signers,
            ))
            .expect("Internal scripting engine inconsistency"),

            ContractDescriptor::MultiSig {
                category: OuterCategory::Hashed,
                threshold,
                signers,
                sorted: false,
            } => Descriptor::new_sh(ContractDescriptor::multisig_miniscript(
                *threshold, signers,
            ))
            .expect("Internal scripting engine inconsistency"),
            ContractDescriptor::MultiSig {
                category: OuterCategory::Hashed,
                threshold,
                signers,
                sorted: true,
            } => Descriptor::new_sh_sortedmulti(*threshold, signers.clone())
                .expect("Internal scripting engine inconsistency"),

            ContractDescriptor::MultiSig {
                category: OuterCategory::SegWit,
                threshold,
                signers,
                sorted: false,
            } => {
                let ms = ContractDescriptor::multisig_miniscript(
                    *threshold, signers,
                );
                if nested {
                    Descriptor::new_sh_wsh(ms)
                        .expect("Too much keys in the multisig")
                } else {
                    Descriptor::new_wsh(ms)
                        .expect("Too much keys in the multisig")
                }
            }
            ContractDescriptor::MultiSig {
                category: OuterCategory::SegWit,
                threshold,
                signers,
                sorted: true,
            } => {
                if nested {
                    Descriptor::new_sh_wsh_sortedmulti(
                        *threshold,
                        signers.clone(),
                    )
                    .expect("Too much keys in the multisig")
                } else {
                    Descriptor::new_wsh_sortedmulti(*threshold, signers.clone())
                        .expect("Too much keys in the multisig")
                }
            }

            ContractDescriptor::MultiSig {
                category: OuterCategory::Taproot,
                ..
            } => panic!("Taproot not yet supported"),

            ContractDescriptor::Script { ms_cache: ms, .. } => {
                ms.to_descriptor()
            }
        }
    }

    pub fn outer_descriptor_type(&self) -> OuterType {
        match self {
            ContractDescriptor::SingleSig { category, .. } => {
                category.into_outer_type(false)
            }
            ContractDescriptor::MultiSig { category, .. } => {
                category.into_outer_type(true)
            }
            ContractDescriptor::Script {
                ms_cache: miniscript,
                ..
            } => miniscript.outer_descriptor_type(),
        }
    }
}

impl<Pk> DescriptorTrait<Pk> for ContractDescriptor<Pk>
where
    Pk: MiniscriptKey + FromStr + StrictEncode + StrictDecode,
    <Pk as FromStr>::Err: Display,
    <Pk as MiniscriptKey>::Hash: FromStr,
    <<Pk as MiniscriptKey>::Hash as FromStr>::Err: Display,
{
    fn sanity_check(&self) -> Result<(), Error> {
        self.to_descriptor(false).sanity_check()
    }

    fn address(&self, network: Network) -> Result<Address, Error>
    where
        Pk: ToPublicKey,
    {
        self.to_descriptor(false).address(network)
    }

    fn script_pubkey(&self) -> Script
    where
        Pk: ToPublicKey,
    {
        self.to_descriptor(false).script_pubkey()
    }

    fn unsigned_script_sig(&self) -> Script
    where
        Pk: ToPublicKey,
    {
        self.to_descriptor(false).unsigned_script_sig()
    }

    fn explicit_script(&self) -> Script
    where
        Pk: ToPublicKey,
    {
        self.to_descriptor(false).explicit_script()
    }

    fn get_satisfaction<S>(
        &self,
        satisfier: S,
    ) -> Result<(Vec<Vec<u8>>, Script), Error>
    where
        Pk: ToPublicKey,
        S: Satisfier<Pk>,
    {
        self.to_descriptor(false).get_satisfaction(satisfier)
    }

    fn max_satisfaction_weight(&self) -> Result<usize, Error> {
        self.to_descriptor(false).max_satisfaction_weight()
    }

    fn script_code(&self) -> Script
    where
        Pk: ToPublicKey,
    {
        self.to_descriptor(false).script_code()
    }
}

impl<Pk> Display for ContractDescriptor<Pk>
where
    Pk: MiniscriptKey + FromStr + StrictEncode + StrictDecode,
    <Pk as FromStr>::Err: Display,
    <Pk as MiniscriptKey>::Hash: FromStr,
    <<Pk as MiniscriptKey>::Hash as FromStr>::Err: Display,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ContractDescriptor::Script { policy, .. } => {
                Display::fmt(policy, f)
            }
            _ => Display::fmt(&self.to_descriptor(f.sign_plus()), f),
        }
    }
}

impl<Pk> FromStr for ContractDescriptor<Pk>
where
    Pk: MiniscriptKey + FromStr + StrictEncode + StrictDecode,
    <Pk as FromStr>::Err: Display,
    <Pk as MiniscriptKey>::Hash: FromStr,
    <<Pk as MiniscriptKey>::Hash as FromStr>::Err: Display,
{
    type Err = miniscript::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match Descriptor::<Pk>::from_str(s)? {
            Descriptor::Pkh(pk) => ContractDescriptor::SingleSig {
                category: OuterCategory::Hashed,
                pk: pk.into_inner(),
            },
            Descriptor::Wpkh(pk) => ContractDescriptor::SingleSig {
                category: OuterCategory::SegWit,
                pk: pk.into_inner(),
            },

            Descriptor::Bare(bare) => {
                let ms = bare.into_inner();
                ContractDescriptor::with(
                    ms,
                    OuterCategory::Bare,
                    &s[5..s.len() - 1],
                )?
            }

            Descriptor::Wsh(wsh) => match wsh.into_inner() {
                WshInner::SortedMulti(SortedMultiVec { k, pks, .. }) => {
                    ContractDescriptor::MultiSig {
                        category: OuterCategory::SegWit,
                        threshold: k,
                        signers: pks,
                        sorted: true,
                    }
                }
                WshInner::Ms(ms) => ContractDescriptor::with(
                    ms,
                    OuterCategory::SegWit,
                    &s[4..s.len() - 1],
                )?,
            },

            Descriptor::Sh(sh) => match sh.into_inner() {
                ShInner::Wsh(_) | ShInner::Wpkh(_) => {
                    ContractDescriptor::from_str(&s[3..s.len() - 1])?
                }
                ShInner::SortedMulti(SortedMultiVec { k, pks, .. }) => {
                    ContractDescriptor::MultiSig {
                        category: OuterCategory::Hashed,
                        threshold: k,
                        signers: pks,
                        sorted: true,
                    }
                }
                ShInner::Ms(ms) => ContractDescriptor::with(
                    ms,
                    OuterCategory::Hashed,
                    &s[3..s.len() - 1],
                )?,
            },
        })
    }
}

#[derive(
    Clone,
    Ord,
    PartialOrd,
    Eq,
    PartialEq,
    Hash,
    Debug,
    Display,
    StrictEncode,
    StrictDecode,
)]
#[display(inner)]
pub enum CompiledMiniscript<Pk>
where
    Pk: MiniscriptKey + FromStr + StrictEncode + StrictDecode,
    <Pk as FromStr>::Err: Display,
    <Pk as MiniscriptKey>::Hash: FromStr,
    <<Pk as MiniscriptKey>::Hash as FromStr>::Err: Display,
{
    Bare(Miniscript<Pk, miniscript::BareCtx>),
    Hashed(Miniscript<Pk, miniscript::Legacy>),
    SegWit(Miniscript<Pk, miniscript::Segwitv0>),
    Taproot(Miniscript<Pk, miniscript::Segwitv0>),
}

impl<Pk> CompiledMiniscript<Pk>
where
    Pk: MiniscriptKey + FromStr + StrictEncode + StrictDecode,
    <Pk as FromStr>::Err: Display,
    <Pk as MiniscriptKey>::Hash: FromStr,
    <<Pk as MiniscriptKey>::Hash as FromStr>::Err: Display,
{
    pub fn with(
        policy: &policy::Concrete<Pk>,
        category: OuterCategory,
    ) -> Result<Self, miniscript::Error> {
        Ok(match category {
            OuterCategory::Bare => CompiledMiniscript::Bare(policy.compile()?),
            OuterCategory::Hashed => {
                CompiledMiniscript::Hashed(policy.compile()?)
            }
            OuterCategory::SegWit => {
                CompiledMiniscript::SegWit(policy.compile()?)
            }
            OuterCategory::Taproot => {
                CompiledMiniscript::Taproot(policy.compile()?)
            }
        })
    }

    pub fn outer_descriptor_type(&self) -> OuterType {
        match self {
            CompiledMiniscript::Bare(_) => OuterType::Bare,
            CompiledMiniscript::Hashed(_) => OuterType::Sh,
            CompiledMiniscript::SegWit(_) => OuterType::Wsh,
            CompiledMiniscript::Taproot(_) => OuterType::Tr,
        }
    }

    pub fn to_descriptor(&self) -> Descriptor<Pk> {
        match self {
            CompiledMiniscript::Bare(ms) => Descriptor::new_bare(ms.clone())
                .expect("Internal script engine inconsistency"),
            CompiledMiniscript::Hashed(ms) => Descriptor::new_sh(ms.clone())
                .expect("Internal script engine inconsistency"),
            CompiledMiniscript::SegWit(ms) => Descriptor::new_wsh(ms.clone())
                .expect("Internal script engine inconsistency"),
            CompiledMiniscript::Taproot(_ms) => {
                panic!("Taproot is not supported yet")
                // Descriptor::new_tr(ms.clone()).expect("Internal script engine
                // inconsistency")
            }
        }
    }
}
