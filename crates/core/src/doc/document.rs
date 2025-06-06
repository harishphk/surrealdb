use crate::ctx::Context;
use crate::ctx::MutableContext;
use crate::dbs::Options;
use crate::dbs::Workable;
use crate::err::Error;
use crate::expr::Base;
use crate::expr::FlowResultExt as _;
use crate::expr::permission::Permission;
use crate::expr::statements::define::DefineDatabaseStatement;
use crate::expr::statements::define::DefineEventStatement;
use crate::expr::statements::define::DefineFieldStatement;
use crate::expr::statements::define::DefineIndexStatement;
use crate::expr::statements::define::DefineTableStatement;
use crate::expr::statements::live::LiveStatement;
use crate::expr::table::Table;
use crate::expr::thing::Thing;
use crate::expr::value::Value;
use crate::iam::Action;
use crate::iam::ResourceKind;
use crate::idx::planner::RecordStrategy;
use crate::idx::planner::iterators::IteratorRecord;
use crate::kvs::cache;
use anyhow::Result;
use reblessive::tree::Stk;
use std::fmt::{Debug, Formatter};
use std::mem;
use std::ops::Deref;
use std::sync::Arc;

pub(crate) struct Document {
	/// The record id of this document
	pub(super) id: Option<Arc<Thing>>,
	/// The table that we should generate a record id from
	pub(super) r#gen: Option<Table>,
	/// Whether this is the second iteration of the processing
	pub(super) retry: bool,
	pub(super) extras: Workable,
	pub(super) initial: CursorDoc,
	pub(super) current: CursorDoc,
	pub(super) initial_reduced: CursorDoc,
	pub(super) current_reduced: CursorDoc,
	pub(super) record_strategy: RecordStrategy,
}

#[non_exhaustive]
#[derive(Clone, Debug)]
pub(crate) struct CursorDoc {
	pub(crate) rid: Option<Arc<Thing>>,
	pub(crate) ir: Option<Arc<IteratorRecord>>,
	pub(crate) doc: CursorValue,
}

#[non_exhaustive]
#[derive(Clone, Debug)]
pub(crate) struct CursorValue {
	mutable: Value,
	read_only: Option<Arc<Value>>,
}

impl CursorValue {
	pub(crate) fn to_mut(&mut self) -> &mut Value {
		if let Some(ro) = self.read_only.take() {
			self.mutable = ro.as_ref().clone();
		}
		&mut self.mutable
	}

	pub(crate) fn as_arc(&mut self) -> Arc<Value> {
		match &self.read_only {
			None => {
				let v = Arc::new(mem::take(&mut self.mutable));
				self.read_only = Some(v.clone());
				v
			}
			Some(v) => v.clone(),
		}
	}

	pub(crate) fn as_ref(&self) -> &Value {
		if let Some(ro) = &self.read_only {
			ro.as_ref()
		} else {
			&self.mutable
		}
	}

	pub(crate) fn into_owned(self) -> Value {
		if let Some(ro) = &self.read_only {
			ro.as_ref().clone()
		} else {
			self.mutable
		}
	}
}

impl Deref for CursorValue {
	type Target = Value;
	fn deref(&self) -> &Self::Target {
		self.as_ref()
	}
}

impl CursorDoc {
	pub(crate) fn new<T: Into<CursorValue>>(
		rid: Option<Arc<Thing>>,
		ir: Option<Arc<IteratorRecord>>,
		doc: T,
	) -> Self {
		Self {
			rid,
			ir,
			doc: doc.into(),
		}
	}
}

impl From<Value> for CursorValue {
	fn from(value: Value) -> Self {
		Self {
			mutable: value,
			read_only: None,
		}
	}
}

impl From<Arc<Value>> for CursorValue {
	fn from(value: Arc<Value>) -> Self {
		Self {
			mutable: Value::None,
			read_only: Some(value),
		}
	}
}

impl From<Value> for CursorDoc {
	fn from(val: Value) -> Self {
		Self {
			rid: None,
			ir: None,
			doc: CursorValue {
				mutable: val,
				read_only: None,
			},
		}
	}
}

impl From<Arc<Value>> for CursorDoc {
	fn from(doc: Arc<Value>) -> Self {
		Self {
			rid: None,
			ir: None,
			doc: CursorValue {
				mutable: Value::None,
				read_only: Some(doc),
			},
		}
	}
}

impl Debug for Document {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "Document - id: <{:?}>", self.id)
	}
}

pub(crate) enum Permitted {
	Initial,
	Current,
	Both,
}

impl Document {
	/// Initialise a new document
	pub fn new(
		id: Option<Arc<Thing>>,
		ir: Option<Arc<IteratorRecord>>,
		r#gen: Option<Table>,
		val: Arc<Value>,
		extras: Workable,
		retry: bool,
		rs: RecordStrategy,
	) -> Self {
		Document {
			id: id.clone(),
			r#gen,
			retry,
			extras,
			current: CursorDoc::new(id.clone(), ir.clone(), val.clone()),
			initial: CursorDoc::new(id.clone(), ir.clone(), val.clone()),
			current_reduced: CursorDoc::new(id.clone(), ir.clone(), val.clone()),
			initial_reduced: CursorDoc::new(id.clone(), ir.clone(), val.clone()),
			record_strategy: rs,
		}
	}

	/// Check if document has changed
	pub fn changed(&self) -> bool {
		self.initial.doc.as_ref() != self.current.doc.as_ref()
	}

	/// Check if document is being created
	pub fn is_new(&self) -> bool {
		self.initial.doc.as_ref().is_none()
	}

	/// Check if this is the first iteration. When
	/// running an UPSERT or INSERT statement we don't
	/// first fetch the value from the storage engine.
	/// If there is an error when attempting to set the
	/// value in the storage engine, then we retry the
	/// document processing, and this will return false.
	pub(crate) fn is_iteration_initial(&self) -> bool {
		!self.retry && self.initial.doc.as_ref().is_none()
	}

	/// Check if the the record id for this document
	/// has been specifically set upfront. This is true
	/// in the following instances:
	///
	/// CREATE some:thing;
	/// CREATE some SET id = some:thing;
	/// CREATE some CONTENT { id: some:thing };
	/// UPSERT some:thing;
	/// UPSERT some SET id = some:thing;
	/// UPSERT some CONTENT { id: some:thing };
	/// INSERT some (id) VALUES (some:thing);
	/// INSERT { id: some:thing };
	/// INSERT [{ id: some:thing }];
	/// RELATE from->some:thing->to;
	/// RELATE from->some->to SET id = some:thing;
	/// RELATE from->some->to CONTENT { id: some:thing };
	///
	/// In addition, when iterating over tables or ranges
	/// the record id will also be specified before we
	/// process the document in this module. So therefore
	/// although this function is not used or checked in
	/// these scenarios, this function will also be true
	/// in the following instances:
	///
	/// UPDATE some;
	/// UPDATE some:thing;
	/// UPDATE some:from..to;
	/// DELETE some;
	/// DELETE some:thing;
	/// DELETE some:from..to;
	pub(crate) fn is_specific_record_id(&self) -> bool {
		match self.extras {
			Workable::Insert(ref v) if v.rid().is_some() => true,
			Workable::Normal if self.r#gen.is_none() => true,
			_ => false,
		}
	}

	/// Retur true if the document has been extracted by an iterator that already matcheed the condition.
	pub(crate) fn is_condition_checked(&self) -> bool {
		matches!(self.record_strategy, RecordStrategy::Count | RecordStrategy::KeysOnly)
	}

	/// Update the document for a retry to update after an insert failed.
	pub fn modify_for_update_retry(&mut self, id: Thing, value: Arc<Value>) {
		let retry = Arc::new(id);
		self.id = Some(retry.clone());
		self.r#gen = None;
		self.retry = true;
		self.record_strategy = RecordStrategy::KeysAndValues;

		self.current = CursorDoc::new(Some(retry), None, value);
		self.initial = self.current.clone();
	}

	/// Checks if permissions are required to be run
	/// over a document. If permissions don't need to
	/// be processed, then we don't process the initial
	/// or current documents, and instead return
	/// `false`. If permissions need to be processed,
	/// then we take the initial or current documents,
	/// and remove those fields which the user is not
	/// allowed to view. We then use the `initial_reduced`
	/// and `current_reduced` documents in the code when
	/// processing the document that a user has access to.
	///
	/// The choice of which documents are reduced can be
	/// specified by passing in a `Permitted` type, allowing
	/// either `initial`, `current`, or `both` to be
	/// processed in a single function execution.
	///
	/// This function is used both to reduce documents
	/// to only the fields that are permitted by updating
	/// the reduced fields of the Document structure as
	/// well as to return whether or not they have been
	/// reduced so that these reduced documents are used
	/// instead of their non-reduced versions.
	///
	/// If there is no requirement to reduce a document
	/// based on the permissions, then this function will
	/// not have any performance impact by cloning the
	/// full and reduced documents.
	pub(crate) async fn reduced(
		&mut self,
		stk: &mut Stk,
		ctx: &Context,
		opt: &Options,
		permitted: Permitted,
	) -> Result<bool> {
		// Check if this record exists
		if self.id.is_none() {
			return Ok(false);
		}
		// Are permissions being skipped?
		if !opt.check_perms(Action::View)? {
			return Ok(false);
		}
		// Fetch the fields for the table
		let fds = self.fd(ctx, opt).await?;
		// Fetch the targets to process
		let targets = match permitted {
			Permitted::Initial => vec![(&self.initial, &mut self.initial_reduced)],
			Permitted::Current => vec![(&self.current, &mut self.current_reduced)],
			Permitted::Both => vec![
				(&self.initial, &mut self.initial_reduced),
				(&self.current, &mut self.current_reduced),
			],
		};
		// Loop over the targets to process
		for target in targets {
			// Get the full document
			let full = target.0;
			// Process the full document
			let mut out =
				full.doc.as_ref().compute(stk, ctx, opt, Some(full)).await.catch_return()?;
			// Loop over each field in document
			for fd in fds.iter() {
				// Loop over each field in document
				for k in out.each(&fd.name).iter() {
					// Process the field permissions
					match &fd.permissions.select {
						Permission::Full => (),
						Permission::None => out.cut(k),
						Permission::Specific(e) => {
							// Disable permissions
							let opt = &opt.new_with_perms(false);
							// Get the initial value
							let val = Arc::new(full.doc.as_ref().pick(k));
							// Configure the context
							let mut ctx = MutableContext::new(ctx);
							ctx.add_value("value", val);
							let ctx = ctx.freeze();
							// Process the PERMISSION clause
							if !e
								.compute(stk, &ctx, opt, Some(full))
								.await
								.catch_return()?
								.is_truthy()
							{
								out.cut(k);
							}
						}
					}
				}
			}
			// Update the permitted document
			target.1.doc = out.into();
		}
		// Return the permitted document
		Ok(true)
	}

	/// Retrieve the record id for this document
	pub fn id(&self) -> Result<Arc<Thing>> {
		match self.id.clone() {
			Some(id) => Ok(id),
			_ => fail!("Expected a document id to be present"),
		}
	}

	/// Retrieve the record id for this document
	pub fn inner_id(&self) -> Result<Thing> {
		match self.id.clone() {
			Some(id) => Ok(Arc::unwrap_or_clone(id)),
			_ => fail!("Expected a document id to be present"),
		}
	}

	/// Get the database for this document
	pub async fn db(&self, ctx: &Context, opt: &Options) -> Result<Arc<DefineDatabaseStatement>> {
		// Get the NS + DB
		let (ns, db) = opt.ns_db()?;
		// Get transaction
		let txn = ctx.tx();
		// Get the table definition
		match ctx.get_cache() {
			// A cache is present on the context
			Some(cache) if txn.local() => {
				// Get the cache entry key
				let key = cache::ds::Lookup::Db(ns, db);
				// Get or update the cache entry
				match cache.get(&key) {
					Some(val) => val,
					None => {
						let val = txn.get_or_add_db(ns, db, opt.strict).await?;
						let val = cache::ds::Entry::Any(val.clone());
						cache.insert(key, val.clone());
						val
					}
				}
				.try_into_type()
			}
			// No cache is present on the context
			_ => txn.get_or_add_db(ns, db, opt.strict).await,
		}
	}

	/// Get the table for this document
	pub async fn tb(&self, ctx: &Context, opt: &Options) -> Result<Arc<DefineTableStatement>> {
		// Get the NS + DB
		let (ns, db) = opt.ns_db()?;
		// Get the record id
		let id = self.id()?;
		// Get transaction
		let txn = ctx.tx();
		// Get the table definition
		match ctx.get_cache() {
			// A cache is present on the context
			Some(cache) if txn.local() => {
				// Get the cache entry key
				let key = cache::ds::Lookup::Tb(ns, db, &id.tb);
				// Get or update the cache entry
				match cache.get(&key) {
					Some(val) => val,
					None => {
						let val = match txn.get_tb(ns, db, &id.tb).await {
							Err(e) => {
								// The table doesn't exist
								if matches!(e.downcast_ref(), Some(Error::TbNotFound { .. })) {
									// Allowed to run?
									opt.is_allowed(Action::Edit, ResourceKind::Table, &Base::Db)?;
									// We can create the table automatically
									txn.ensure_ns_db_tb(ns, db, &id.tb, opt.strict).await
								} else {
									// There was an error
									Err(e)
								}
							}
							// The table exists
							Ok(tb) => Ok(tb),
						}?;
						let val = cache::ds::Entry::Any(val.clone());
						cache.insert(key, val.clone());
						val
					}
				}
				.try_into_type()
			}
			// No cache is present on the context
			_ => {
				// Return the table or attempt to define it
				match txn.get_tb(ns, db, &id.tb).await {
					Err(e) => {
						// The table doesn't exist
						if matches!(e.downcast_ref(), Some(Error::TbNotFound { .. })) {
							// Allowed to run?
							opt.is_allowed(Action::Edit, ResourceKind::Table, &Base::Db)?;
							// We can create the table automatically
							txn.ensure_ns_db_tb(ns, db, &id.tb, opt.strict).await
						} else {
							// There was an error
							Err(e)
						}
					}
					// The table exists
					Ok(tb) => Ok(tb),
				}
			}
		}
	}

	/// Get the foreign tables for this document
	pub async fn ft(&self, ctx: &Context, opt: &Options) -> Result<Arc<[DefineTableStatement]>> {
		// Get the NS + DB
		let (ns, db) = opt.ns_db()?;
		// Get the document table
		let tb = self.tb(ctx, opt).await?;
		// Get the cache from the context
		match ctx.get_cache() {
			// A cache is present on the context
			Some(cache) => {
				// Get the cache entry key
				let key = cache::ds::Lookup::Fts(ns, db, &tb.name, tb.cache_tables_ts);
				// Get or update the cache entry
				match cache.get(&key) {
					Some(val) => val,
					None => {
						let val = ctx.tx().all_tb_views(ns, db, &tb.name).await?;
						let val = cache::ds::Entry::Fts(val.clone());
						cache.insert(key, val.clone());
						val
					}
				}
			}
			.try_into_fts(),
			// No cache is present on the context
			None => ctx.tx().all_tb_views(ns, db, &tb.name).await,
		}
	}

	/// Get the events for this document
	pub async fn ev(&self, ctx: &Context, opt: &Options) -> Result<Arc<[DefineEventStatement]>> {
		// Get the NS + DB
		let (ns, db) = opt.ns_db()?;
		// Get the document table
		let tb = self.tb(ctx, opt).await?;
		// Get the cache from the context
		match ctx.get_cache() {
			// A cache is present on the context
			Some(cache) => {
				// Get the cache entry key
				let key = cache::ds::Lookup::Evs(ns, db, &tb.name, tb.cache_events_ts);
				// Get or update the cache entry
				match cache.get(&key) {
					Some(val) => val,
					None => {
						let val = ctx.tx().all_tb_events(ns, db, &tb.name).await?;
						let val = cache::ds::Entry::Evs(val.clone());
						cache.insert(key, val.clone());
						val
					}
				}
			}
			.try_into_evs(),
			// No cache is present on the context
			None => ctx.tx().all_tb_events(ns, db, &tb.name).await,
		}
	}

	/// Get the fields for this document
	pub async fn fd(&self, ctx: &Context, opt: &Options) -> Result<Arc<[DefineFieldStatement]>> {
		// Get the NS + DB
		let (ns, db) = opt.ns_db()?;
		// Get the document table
		let tb = self.tb(ctx, opt).await?;
		// Get the cache from the context
		match ctx.get_cache() {
			// A cache is present on the context
			Some(cache) => {
				// Get the cache entry key
				let key = cache::ds::Lookup::Fds(ns, db, &tb.name, tb.cache_fields_ts);
				// Get or update the cache entry
				match cache.get(&key) {
					Some(val) => val,
					None => {
						let val = ctx.tx().all_tb_fields(ns, db, &tb.name, opt.version).await?;
						let val = cache::ds::Entry::Fds(val.clone());
						cache.insert(key, val.clone());
						val
					}
				}
			}
			.try_into_fds(),
			// No cache is present on the context
			None => ctx.tx().all_tb_fields(ns, db, &tb.name, opt.version).await,
		}
	}

	/// Get the indexes for this document
	pub async fn ix(&self, ctx: &Context, opt: &Options) -> Result<Arc<[DefineIndexStatement]>> {
		// Get the NS + DB
		let (ns, db) = opt.ns_db()?;
		// Get the document table
		let tb = self.tb(ctx, opt).await?;
		// Get the cache from the context
		match ctx.get_cache() {
			// A cache is present on the context
			Some(cache) => {
				// Get the cache entry key
				let key = cache::ds::Lookup::Ixs(ns, db, &tb.name, tb.cache_indexes_ts);
				// Get or update the cache entry
				match cache.get(&key) {
					Some(val) => val,
					None => {
						let val = ctx.tx().all_tb_indexes(ns, db, &tb.name).await?;
						let val = cache::ds::Entry::Ixs(val.clone());
						cache.insert(key, val.clone());
						val
					}
				}
			}
			.try_into_ixs(),
			// No cache is present on the context
			None => ctx.tx().all_tb_indexes(ns, db, &tb.name).await,
		}
	}

	// Get the lives for this document
	pub async fn lv(&self, ctx: &Context, opt: &Options) -> Result<Arc<[LiveStatement]>> {
		// Get the NS + DB
		let (ns, db) = opt.ns_db()?;
		// Get the document table
		let tb = self.tb(ctx, opt).await?;
		// Get the cache from the context
		match ctx.get_cache() {
			// A cache is present on the context
			Some(cache) => {
				// Get the live-queries cache version
				let version = cache.get_live_queries_version(ns, db, &tb.name)?;
				// Get the cache entry key
				let key = cache::ds::Lookup::Lvs(ns, db, &tb.name, version);
				// Get or update the cache entry
				match cache.get(&key) {
					Some(val) => val,
					None => {
						let val = ctx.tx().all_tb_lives(ns, db, &tb.name).await?;
						let val = cache::ds::Entry::Lvs(val.clone());
						cache.insert(key, val.clone());
						val
					}
				}
				.try_into_lvs()
			}
			// No cache is present on the context
			None => ctx.tx().all_tb_lives(ns, db, &tb.name).await,
		}
	}
}
