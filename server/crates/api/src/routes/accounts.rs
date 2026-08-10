//! Accounts, roles and profiles (ADR-0034 items 1, 3, 7; ADR-0040 items 1, 3).

use crate::authority::{Caller, INSUFFICIENT_ROLE};
use crate::error::{ApiError, ApiResult, blocking};
use crate::routes::auth::{AccountDto, ProfileDto, account_dto, profile_dto};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use nightjar_auth::{hash_password, mint_profile_ref};
use nightjar_core::Role;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsResponse {
    pub accounts: Vec<AccountDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilesResponse {
    pub profiles: Vec<ProfileDto>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAccountRequest {
    pub username: String,
    pub password: String,
    pub role: String,
    pub profile_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRoleRequest {
    pub role: String,
    #[serde(default)]
    pub confirm_demotes_caller: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileQuery {
    pub account_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProfileRequest {
    pub name: String,
    pub account_id: Option<i64>,
    pub classification_cap: Option<String>,
    #[serde(default)]
    pub simple_interface: bool,
}

/// What a role change resolves to once authority is settled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RoleChange {
    /// Demote the acting owner and promote the target, in one transaction.
    Transfer,
    Set(Role),
}

/// Authority for changing an account's role (ADR-0040 item 3).
///
/// A function rather than inline handler code so the three owner-only actions
/// can be tested without standing up an `AppState`, which needs spawned worker
/// threads and an ffmpeg probe.
pub(crate) fn authorize_role_change(
    caller: &Caller,
    target_id: i64,
    requested: Role,
    confirm_demotes_caller: bool,
) -> ApiResult<RoleChange> {
    if !caller.is_owner() {
        return Err(ApiError::forbidden(
            "owner_only: only the owner may change a role",
        ));
    }
    // Before anything else, and it is a different class of refusal: an owner
    // setting their own role to `member` leaves the server ownerless, and the
    // partial unique index does not object because it forbids two owners and
    // not none (ADR-0040 item 1 as amended 2026-08-10).
    if target_id == caller.session.account_id {
        return Err(ApiError::bad_request(
            "cannot_change_own_role: transfer ownership to change who owns the server",
        ));
    }
    if requested.is_owner() {
        if !confirm_demotes_caller {
            return Err(ApiError::bad_request(
                "confirm_required: setting role owner transfers ownership and demotes you to \
                 manager; resend with confirmDemotesCaller true",
            ));
        }
        return Ok(RoleChange::Transfer);
    }
    Ok(RoleChange::Set(requested))
}

/// Authority for deleting an account (ADR-0034 item 7, ADR-0040 item 3).
pub(crate) fn authorize_account_delete(caller: &Caller, target_role: Role) -> ApiResult<()> {
    caller.require_account_powers()?;
    if target_role.is_owner() {
        return Err(ApiError::forbidden(
            "owner_is_unremovable: transfer ownership first",
        ));
    }
    // The eviction case the role model exists for: under a boolean, anyone who
    // could administer the server could remove the person who installed it.
    if target_role.has_account_powers() && !caller.is_owner() {
        return Err(ApiError::forbidden(
            "owner_only: only the owner may delete a manager",
        ));
    }
    Ok(())
}

pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
) -> ApiResult<Json<AccountsResponse>> {
    blocking(move || {
        caller.require_account_powers()?;
        let rows = state
            .db
            .with_conn(nightjar_db::list_accounts)
            .map_err(ApiError::internal)?;
        let accounts = rows
            .iter()
            .map(account_dto)
            .collect::<ApiResult<Vec<_>>>()?;
        Ok(Json(AccountsResponse { accounts }))
    })
    .await
}

pub async fn create(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<CreateAccountRequest>,
) -> ApiResult<(StatusCode, Json<AccountDto>)> {
    blocking(move || {
        caller.require_account_powers()?;
        let role = Role::parse(&body.role)
            .ok_or_else(|| ApiError::bad_request("role must be manager or member"))?;
        // There is exactly one owner and the only way to become it is
        // transfer, which is an owner-only action (ADR-0040 item 3). Refusing
        // here as well as at the index keeps the error legible.
        if role.is_owner() {
            return Err(ApiError::forbidden(
                "owner_is_singleton: transfer ownership instead of creating a second owner",
            ));
        }
        let username = body.username.trim();
        if username.is_empty() || body.password.is_empty() {
            return Err(ApiError::bad_request("username and password are required"));
        }
        let hash = hash_password(&body.password)
            .map_err(|e| ApiError::internal(format!("hash password: {e:?}")))?;
        let profile_name = body.profile_name.as_deref().unwrap_or(username);
        let account = state
            .db
            .with_conn(|conn| {
                nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    role.as_str(),
                    profile_name,
                    &mint_profile_ref(),
                )?;
                nightjar_db::account_by_username(conn, username)?
                    .ok_or_else(|| "account vanished after creation".to_string())
            })
            .map_err(|e| {
                if e.contains("UNIQUE") {
                    ApiError::conflict("username_taken: that username already exists")
                } else {
                    ApiError::internal(e)
                }
            })?;
        Ok((StatusCode::CREATED, Json(account_dto(&account)?)))
    })
    .await
}

pub async fn delete(
    State(state): State<AppState>,
    caller: Caller,
    Path(account_id): Path<i64>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        caller.require_account_powers()?;
        let target = state
            .db
            .with_conn(|conn| nightjar_db::account_by_id(conn, account_id))
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("account {account_id} not found")))?;
        let target_role = Role::parse(&target.role)
            .ok_or_else(|| ApiError::internal(format!("account {account_id} has a bad role")))?;
        authorize_account_delete(&caller, target_role)?;
        state
            .db
            .with_conn(|conn| nightjar_db::delete_account(conn, account_id))
            .map_err(ApiError::internal)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub async fn set_role(
    State(state): State<AppState>,
    caller: Caller,
    Path(account_id): Path<i64>,
    Json(body): Json<SetRoleRequest>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        let role = Role::parse(&body.role)
            .ok_or_else(|| ApiError::bad_request("role must be owner, manager or member"))?;
        let change = authorize_role_change(&caller, account_id, role, body.confirm_demotes_caller)?;
        state
            .db
            .with_conn(|conn| nightjar_db::account_by_id(conn, account_id))
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("account {account_id} not found")))?;

        match change {
            RoleChange::Transfer => state
                .db
                .with_conn(|conn| {
                    nightjar_db::transfer_ownership(conn, caller.session.account_id, account_id)
                })
                .map_err(ApiError::internal)?,
            RoleChange::Set(role) => state
                .db
                .with_conn(|conn| nightjar_db::set_role(conn, account_id, role.as_str()))
                .map_err(ApiError::internal)?,
        }
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub async fn list_profiles(
    State(state): State<AppState>,
    caller: Caller,
    Query(query): Query<ProfileQuery>,
) -> ApiResult<Json<ProfilesResponse>> {
    blocking(move || {
        let account_id = query.account_id.unwrap_or(caller.session.account_id);
        // A member asking about another account is refused, and refused
        // identically whether or not that account exists. An empty list would
        // be indistinguishable from "exists with no profiles", and a 404 would
        // separate the two; both are account-enumeration oracles.
        if !caller.may_act_on_account(account_id) {
            return Err(ApiError::forbidden(INSUFFICIENT_ROLE));
        }
        let rows = state
            .db
            .with_conn(|conn| nightjar_db::profiles_for_account(conn, account_id))
            .map_err(ApiError::internal)?;
        Ok(Json(ProfilesResponse {
            profiles: rows.iter().map(profile_dto).collect(),
        }))
    })
    .await
}

pub async fn create_profile(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<CreateProfileRequest>,
) -> ApiResult<(StatusCode, Json<ProfileDto>)> {
    blocking(move || {
        let account_id = body.account_id.unwrap_or(caller.session.account_id);
        if !caller.may_act_on_account(account_id) {
            return Err(ApiError::forbidden(INSUFFICIENT_ROLE));
        }
        if body.name.trim().is_empty() {
            return Err(ApiError::bad_request("name is required"));
        }
        let profile_ref = mint_profile_ref();
        let row = state
            .db
            .with_conn(|conn| {
                nightjar_db::create_profile(
                    conn,
                    account_id,
                    &profile_ref,
                    body.name.trim(),
                    body.classification_cap.as_deref(),
                    body.simple_interface,
                )?;
                nightjar_db::profile_by_ref(conn, &profile_ref)?
                    .ok_or_else(|| "profile vanished after creation".to_string())
            })
            .map_err(ApiError::internal)?;
        Ok((StatusCode::CREATED, Json(profile_dto(&row))))
    })
    .await
}

pub async fn delete_profile(
    State(state): State<AppState>,
    caller: Caller,
    Path(profile_ref): Path<String>,
) -> ApiResult<StatusCode> {
    blocking(move || {
        let profile = state
            .db
            .with_conn(|conn| nightjar_db::profile_by_ref(conn, &profile_ref))
            .map_err(ApiError::internal)?;
        // Refused identically for a profile that does not exist and one on
        // another account, so the response cannot be used to probe.
        let Some(profile) = profile.filter(|p| caller.may_act_on_account(p.account_id)) else {
            return Err(ApiError::forbidden(INSUFFICIENT_ROLE));
        };
        state
            .db
            .with_conn(|conn| nightjar_db::delete_profile(conn, profile.id))
            .map_err(ApiError::internal)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::SessionRow;

    fn caller(account_id: i64, role: Role, narrowed: bool) -> Caller {
        Caller {
            session: SessionRow {
                id: 1,
                account_id,
                role: role.as_str().to_string(),
                active_profile_id: narrowed.then_some(1),
                client_label: "test".into(),
                expires_at: "2099-01-01T00:00:00.000Z".into(),
                revoked_at: None,
            },
            role,
        }
    }

    fn owner() -> Caller {
        caller(1, Role::Owner, false)
    }
    fn manager() -> Caller {
        caller(2, Role::Manager, false)
    }

    // ADR-0040 item 3 names exactly three owner-only actions. Each gets its own
    // test rather than one parameterised over the set: if they diverge later,
    // a single test tells you something broke but not which of the three.

    /// Owner-only action 1: transfer ownership.
    #[test]
    fn a_manager_cannot_transfer_ownership() {
        let refused = authorize_role_change(&manager(), 9, Role::Owner, true).unwrap_err();
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert!(refused.message.starts_with("owner_only:"), "{refused:?}");

        // And the owner can, which is what makes the refusal specific to role
        // rather than to the action being rejected outright.
        assert_eq!(
            authorize_role_change(&owner(), 9, Role::Owner, true).unwrap(),
            RoleChange::Transfer
        );
    }

    /// Owner-only action 2: change any account's role.
    #[test]
    fn a_manager_cannot_change_a_role() {
        let refused = authorize_role_change(&manager(), 9, Role::Manager, false).unwrap_err();
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert!(refused.message.starts_with("owner_only:"), "{refused:?}");

        assert_eq!(
            authorize_role_change(&owner(), 9, Role::Member, false).unwrap(),
            RoleChange::Set(Role::Member)
        );
    }

    /// Owner-only action 3: delete an account whose role is `manager`.
    #[test]
    fn a_manager_cannot_delete_a_manager() {
        let refused = authorize_account_delete(&manager(), Role::Manager).unwrap_err();
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert!(refused.message.starts_with("owner_only:"), "{refused:?}");

        // A manager deleting a *member* is an ordinary account power, so the
        // refusal is about the target's role and not about deletion.
        assert!(authorize_account_delete(&manager(), Role::Member).is_ok());
        assert!(authorize_account_delete(&owner(), Role::Manager).is_ok());
    }

    /// The owner is unremovable regardless of who asks, which is a different
    /// refusal from the three above and carries its own name.
    #[test]
    fn nobody_can_delete_the_owner() {
        for actor in [owner(), manager()] {
            let refused = authorize_account_delete(&actor, Role::Owner).unwrap_err();
            assert!(
                refused.message.starts_with("owner_is_unremovable:"),
                "{refused:?}"
            );
        }
    }

    /// ADR-0040 item 1 as amended: the owner cannot demote themselves. The
    /// partial unique index does not catch this, because zero owners violates
    /// nothing, so the refusal has to be here.
    #[test]
    fn the_owner_cannot_demote_themselves() {
        let owner = owner();
        let own_id = owner.session.account_id;
        for requested in [Role::Member, Role::Manager, Role::Owner] {
            let refused = authorize_role_change(&owner, own_id, requested, true).unwrap_err();
            assert!(
                refused.message.starts_with("cannot_change_own_role:"),
                "{requested:?} -> {refused:?}"
            );
        }
    }

    /// The confirmation is required for transfer and ignored otherwise, so an
    /// ordinary role change is not made tedious by a flag that only guards the
    /// consequential case.
    #[test]
    fn transfer_needs_confirmation_and_other_changes_do_not() {
        let refused = authorize_role_change(&owner(), 9, Role::Owner, false).unwrap_err();
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert!(
            refused.message.starts_with("confirm_required:"),
            "{refused:?}"
        );
        assert!(authorize_role_change(&owner(), 9, Role::Manager, false).is_ok());
    }

    /// ADR-0034 item 3: a profile session never administers the server, even
    /// when the account behind it is the owner.
    #[test]
    fn a_narrowed_session_has_no_authority() {
        let narrowed_owner = caller(1, Role::Owner, true);
        assert!(!narrowed_owner.is_owner());
        assert!(!narrowed_owner.has_account_powers());
        assert!(
            authorize_role_change(&narrowed_owner, 9, Role::Manager, false).is_err(),
            "an owner acting as a profile is not acting as the owner"
        );
        assert!(authorize_account_delete(&narrowed_owner, Role::Member).is_err());
    }
}
