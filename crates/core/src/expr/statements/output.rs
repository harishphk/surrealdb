use crate::dbs::Options;
use crate::doc::CursorDoc;
use crate::expr::ControlFlow;
use crate::expr::fetch::Fetchs;
use crate::expr::value::Value;
use crate::{ctx::Context, expr::FlowResult};

use reblessive::tree::Stk;
use revision::revisioned;
use serde::{Deserialize, Serialize};
use std::fmt;

#[revisioned(revision = 1)]
#[derive(Clone, Debug, Default, Eq, PartialEq, PartialOrd, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub struct OutputStatement {
	pub what: Value,
	pub fetch: Option<Fetchs>,
}

impl OutputStatement {
	/// Check if we require a writeable transaction
	pub(crate) fn writeable(&self) -> bool {
		self.what.writeable()
	}
	/// Process this type returning a computed simple Value
	pub(crate) async fn compute(
		&self,
		stk: &mut Stk,
		ctx: &Context,
		opt: &Options,
		doc: Option<&CursorDoc>,
	) -> FlowResult<Value> {
		// Ensure futures are processed
		let opt = &opt.new_with_futures(true);
		// Process the output value
		let mut value = self.what.compute(stk, ctx, opt, doc).await?;
		// Fetch any
		if let Some(fetchs) = &self.fetch {
			let mut idioms = Vec::with_capacity(fetchs.0.len());
			for fetch in fetchs.iter() {
				fetch.compute(stk, ctx, opt, &mut idioms).await?
			}
			for i in &idioms {
				value.fetch(stk, ctx, opt, i).await?;
			}
		}
		//
		Err(ControlFlow::Return(value))
	}
}

impl fmt::Display for OutputStatement {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "RETURN {}", self.what)?;
		if let Some(ref v) = self.fetch {
			write!(f, " {v}")?
		}
		Ok(())
	}
}
