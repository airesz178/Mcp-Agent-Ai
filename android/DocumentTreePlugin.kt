package ai.mcpagent.app

import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.DocumentsContract
import android.util.Base64
import androidx.activity.result.ActivityResult
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.text.Normalizer
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import org.json.JSONArray
import org.json.JSONObject

@InvokeArg
class DocumentTreeArgs {
  lateinit var treeUri: String
  var path: List<String> = emptyList()
}

@InvokeArg
class DocumentTreeWriteRequestArgs {
  var userInitiated: Boolean = false
}

@InvokeArg
class DocumentTreeCreateTextArgs {
  lateinit var treeUri: String
  var path: List<String> = emptyList()
  lateinit var name: String
  lateinit var content: String
  var userInitiated: Boolean = false
}

@InvokeArg
class DocumentTreeCreateFolderArgs {
  lateinit var treeUri: String
  var path: List<String> = emptyList()
  lateinit var name: String
  var userInitiated: Boolean = false
}

@InvokeArg
class DocumentTreeRenameEntryArgs {
  lateinit var treeUri: String
  var path: List<String> = emptyList()
  lateinit var name: String
  var userInitiated: Boolean = false
}

@InvokeArg
class DocumentTreePropertiesArgs {
  lateinit var treeUri: String
  var path: List<String> = emptyList()
  var userInitiated: Boolean = false
}

@InvokeArg
class DocumentTreeProjectFolderArgs {
  lateinit var projectId: String
  var userInitiated: Boolean = false
}

/**
 * A single user-selected Storage Access Framework tree. This plugin never asks
 * for broad storage access and never resolves filesystem paths. Each document
 * is resolved from children of the granted tree, one name at a time.
 *
 * Commands: status(), request(), list({treeUri,path}),
 * openDocument({treeUri,path}). `path` is a list of child display names;
 * [] addresses the root for list(). openDocument returns bounded base64 bytes.
 * Optional write workflow: requestWrite({userInitiated:true}) requests a
 * separate write grant for the SAME selected tree; createText({...}) confirms
 * one UTF-8 text/plain creation (max 64 KiB) with the user before writing.
 * createFolder({treeUri,path,name,userInitiated:true}) confirms creation of
 * one NEW directory; the provider must verify its exact ID/name/parent.
 * renameEntry({treeUri,path,name,userInitiated:true}) renames only the final
 * existing entry within the same selected parent and verifies provider state.
 * properties({treeUri,path,userInitiated:true}) queries provider metadata and
 * flags without reading document content or requiring a WRITE grant.
 */
@TauriPlugin
class DocumentTreePlugin(private val activity: Activity) : Plugin(activity) {
  companion object {
    private const val PREFS = "cos_document_tree_v1"
    private const val TREE_KEY = "selected_tree"
    private const val PROJECT_TREE_PREFIX = "project_tree:"
    private const val PROJECT_NAME_PREFIX = "project_name:"
    private const val MAX_PROJECT_FOLDERS = 30
    private const val MAX_DEPTH = 16
    private const val MAX_ENTRIES = 128
    private const val MAX_NAME_LENGTH = 255
    private const val MAX_DOCUMENT_BYTES = 1_048_576
    private const val MAX_CREATE_BYTES = 65_536
    private const val MAX_DOCUMENT_ID_LENGTH = 4096
    private const val MAX_SAFE_NUMBER = 9_007_199_254_740_991L
    // Maximum representable UTC date through year 9999, in milliseconds.
    private const val MAX_PROVIDER_TIMESTAMP_MS = 253_402_300_799_999L
    private const val DIRECTORY = DocumentsContract.Document.MIME_TYPE_DIR
  }

  private data class Entry(
    val documentId: String,
    val name: String,
    val mimeType: String,
    val size: Long
  ) {
    val isDirectory: Boolean get() = mimeType == DIRECTORY
  }

  private val pickerActive = AtomicBoolean(false)
  private val writeConfirmationActive = AtomicBoolean(false)
  private val folderCreationActive = AtomicBoolean(false)
  private val renameConfirmationActive = AtomicBoolean(false)
  private val worker = ThreadPoolExecutor(
    1, 1, 0L, TimeUnit.MILLISECONDS, ArrayBlockingQueue<Runnable>(2),
    { task -> Thread(task, "cos-document-tree-io").apply { isDaemon = true } },
    ThreadPoolExecutor.AbortPolicy()
  )

  private fun supported(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP

  /** isTreeUri() also accepts tree-backed *document* URIs. Only a canonical
   * picker tree root may identify the persisted selection; never treat an
   * arbitrary nested document URI as a new tree grant. */
  private fun canonicalSelectedTree(uri: Uri): Boolean {
    return try {
      if (uri.toString().length !in 1..8192 || uri.scheme != "content" ||
        uri.authority.isNullOrBlank() || !DocumentsContract.isTreeUri(uri))
        return false
      val id = DocumentsContract.getTreeDocumentId(uri)
      id.length in 1..MAX_DOCUMENT_ID_LENGTH &&
        uri == DocumentsContract.buildTreeDocumentUri(uri.authority!!, id)
    } catch (_: Exception) {
      false
    }
  }

  private fun storedUri(): Uri? {
    val raw = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
      .getString(TREE_KEY, null) ?: return null
    return try {
      val uri = Uri.parse(raw)
      if (!canonicalSelectedTree(uri)) null else uri
    } catch (_: Exception) {
      null
    }
  }

  private fun hasReadGrant(uri: Uri): Boolean =
    activity.contentResolver.persistedUriPermissions.any {
      it.isReadPermission && it.uri == uri
    }

  private fun hasWriteGrant(uri: Uri): Boolean =
    activity.contentResolver.persistedUriPermissions.any {
      it.isWritePermission && it.uri == uri
    }

  private fun activeTree(): Uri? {
    val uri = storedUri() ?: return null
    return if (hasReadGrant(uri)) uri else null
  }

  private fun validProjectId(id: String): Boolean =
    (id.length == 36 && Regex("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").matches(id)) ||
    (id.length in 9..90 && id.startsWith("project-") &&
      id.all { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' || it == '-' || it == '_' })

  private fun projectUri(id: String): Uri? {
    val raw = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
      .getString(PROJECT_TREE_PREFIX + id, null) ?: return null
    return try { Uri.parse(raw).takeIf(::canonicalSelectedTree) }
      catch (_: Exception) { null }
  }

  private fun projectFolderCount(): Int =
    activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE).all.keys.count {
      it.startsWith(PROJECT_TREE_PREFIX)
    }

  private fun treeStillReferenced(uri: Uri): Boolean {
    if (storedUri() == uri) return true
    return activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE).all.entries.any {
      it.key.startsWith(PROJECT_TREE_PREFIX) && it.value == uri.toString()
    }
  }

  /** An independent READ-only SAF binding for each existing Project label.
   * The returned URI is metadata, not a private device filesystem path. */
  @Command
  fun projectFolderStatus(invoke: Invoke) {
    val args = try { invoke.parseArgs(DocumentTreeProjectFolderArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid Project folder status request"); return }
    if (!validProjectId(args.projectId)) {
      invoke.reject("Invalid Project ID")
      return
    }
    val uri = projectUri(args.projectId)
    val granted = uri != null && hasReadGrant(uri)
    val name = if (granted) activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
      .getString(PROJECT_NAME_PREFIX + args.projectId, "") ?: "" else ""
    invoke.resolve(JSObject().apply {
      put("projectId", args.projectId)
      put("linked", uri != null)
      put("granted", granted)
      put("treeUri", if (granted) uri.toString() else "")
      put("displayName", name)
      put("pickerOpen", pickerActive.get())
      put("readOnly", true)
    })
  }

  /** The explicit picker never touches the Files tab's selected tree. */
  @Command
  fun requestProjectFolder(invoke: Invoke) {
    val args = try { invoke.parseArgs(DocumentTreeProjectFolderArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid Project folder request"); return }
    if (!validProjectId(args.projectId) || !args.userInitiated) {
      invoke.reject("Project folder selection requires a valid Project and user action")
      return
    }
    if (!supported() || !pickerActive.compareAndSet(false, true)) {
      invoke.reject("Android Project folder picker is unavailable or already open")
      return
    }
    try {
      val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        addFlags(Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION)
        val previous = projectUri(args.projectId)
        if (previous != null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
          putExtra(DocumentsContract.EXTRA_INITIAL_URI, previous)
      }
      startActivityForResult(invoke, intent, "projectFolderSelected")
    } catch (_: Exception) {
      pickerActive.set(false)
      invoke.reject("Could not open Android Project folder picker")
    }
  }

  @ActivityCallback
  private fun projectFolderSelected(invoke: Invoke, result: ActivityResult) {
    var newlyGranted: Uri? = null
    var saved = false
    try {
      val args = invoke.parseArgs(DocumentTreeProjectFolderArgs::class.java)
      if (!args.userInitiated || !validProjectId(args.projectId))
        throw SecurityException("Invalid Project folder selection")
      if (result.resultCode != Activity.RESULT_OK) {
        invoke.resolve(JSObject().apply {
          put("projectId", args.projectId)
          put("selected", false)
          put("cancelled", true)
        })
        return
      }
      val data = result.data
      val uri = data?.data
      if (uri == null || !canonicalSelectedTree(uri) ||
        (data.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION) == 0)
        throw SecurityException("Invalid selected Project folder")
      val previous = projectUri(args.projectId)
      if (previous == null && projectFolderCount() >= MAX_PROJECT_FOLDERS)
        throw IOException("Maximum Project folder bindings reached")
      activity.contentResolver.takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION)
      newlyGranted = uri
      if (!hasReadGrant(uri)) throw SecurityException("Project folder read access was not persisted")
      val metadata = propertyMetadata(uri, DocumentsContract.getTreeDocumentId(uri))
      if (!metadata.isDirectory) throw IOException("Selected Project location is not a directory")
      if (!hasReadGrant(uri)) throw SecurityException("Project folder read access was revoked")
      val prefs = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
      saved = prefs.edit()
        .putString(PROJECT_TREE_PREFIX + args.projectId, uri.toString())
        .putString(PROJECT_NAME_PREFIX + args.projectId, metadata.name)
        .commit()
      if (!saved) throw IOException("Project folder binding could not be saved")
      if (previous != null && previous != uri) releaseOwnGrant(previous)
      invoke.resolve(JSObject().apply {
        put("projectId", args.projectId)
        put("selected", true)
        put("granted", true)
        put("persisted", true)
        put("treeUri", uri.toString())
        put("displayName", metadata.name)
        put("readOnly", true)
      })
    } catch (_: Exception) {
      invoke.reject("Could not verify or persist the selected Project folder")
    } finally {
      if (!saved && newlyGranted != null) releaseOwnGrant(newlyGranted)
      pickerActive.set(false)
    }
  }

  /** Removes one Project association. Only release a URI no longer referenced
   * by Files or another Project; this never deletes a folder or its contents. */
  @Command
  fun releaseProjectFolder(invoke: Invoke) {
    val args = try { invoke.parseArgs(DocumentTreeProjectFolderArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid Project unlink request"); return }
    if (!validProjectId(args.projectId) || !args.userInitiated || pickerActive.get()) {
      invoke.reject("Project unlink requires an explicit user action and no active picker")
      return
    }
    val old = projectUri(args.projectId)
    val prefs = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
    if (!prefs.edit().remove(PROJECT_TREE_PREFIX + args.projectId)
      .remove(PROJECT_NAME_PREFIX + args.projectId).commit()) {
      invoke.reject("Could not unlink Project folder")
      return
    }
    if (old != null) releaseOwnGrant(old)
    invoke.resolve(JSObject().apply {
      put("projectId", args.projectId)
      put("released", old != null)
      put("linked", false)
      put("filesDeleted", false)
    })
  }

  private fun responseStatus(): JSObject {
    val active = activeTree()
    return JSObject().apply {
      put("supported", supported())
      put("granted", active != null)
      put("writeGranted", active != null && hasWriteGrant(active))
      put("treeUri", active?.toString() ?: "")
      put("pickerOpen", pickerActive.get())
      put("maxEntries", MAX_ENTRIES)
      put("maxDepth", MAX_DEPTH)
      put("maxReadBytes", MAX_DOCUMENT_BYTES)
      put("maxCreateBytes", MAX_CREATE_BYTES)
    }
  }

  @Command
  fun status(invoke: Invoke) {
    invoke.resolve(responseStatus())
  }

  /** Only an explicit UI call can open the system folder picker. */
  @Command
  fun request(invoke: Invoke) {
    if (!supported()) {
      invoke.reject("Android document-tree selection is unavailable")
      return
    }
    if (!pickerActive.compareAndSet(false, true)) {
      invoke.reject("Document-tree picker is already open")
      return
    }
    try {
      val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        addFlags(Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION)
      }
      startActivityForResult(invoke, intent, "treeSelected")
    } catch (_: Exception) {
      pickerActive.set(false)
      invoke.reject("Could not open Android document-tree picker")
    }
  }

  /**
   * The existing read-only tree selection stays intact. The Android picker
   * must explicitly grant WRITE on that exact folder; picking anything else
   * is rejected without persisting a grant for another location.
   */
  @Command
  fun requestWrite(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(DocumentTreeWriteRequestArgs::class.java)
    } catch (_: Exception) {
      invoke.reject("Invalid document-tree write request")
      return
    }
    if (!args.userInitiated) {
      invoke.reject("Document-tree write selection requires a user gesture")
      return
    }
    val selected = activeTree()
    if (!supported() || selected == null) {
      invoke.reject("Select a readable document tree before requesting write access")
      return
    }
    if (!pickerActive.compareAndSet(false, true)) {
      invoke.reject("Document-tree picker is already open")
      return
    }
    try {
      val intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE).apply {
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
        addFlags(Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O)
          putExtra(DocumentsContract.EXTRA_INITIAL_URI, selected)
      }
      startActivityForResult(invoke, intent, "writeTreeSelected")
    } catch (_: Exception) {
      pickerActive.set(false)
      invoke.reject("Could not open Android write-access folder picker")
    }
  }

  @ActivityCallback
  private fun writeTreeSelected(invoke: Invoke, result: ActivityResult) {
    try {
      if (result.resultCode != Activity.RESULT_OK) {
        invoke.resolve(JSObject().apply {
          put("selected", false)
          put("cancelled", true)
          put("writeGranted", false)
        })
        return
      }
      val selected = activeTree()
      val returned = result.data?.data
      if (selected == null || returned == null || returned != selected ||
        !canonicalSelectedTree(returned) ||
        (result.data!!.flags and Intent.FLAG_GRANT_WRITE_URI_PERMISSION) == 0) {
        invoke.reject("Choose the same previously selected folder and grant write access")
        return
      }
      activity.contentResolver.takePersistableUriPermission(
        returned, Intent.FLAG_GRANT_WRITE_URI_PERMISSION
      )
      if (!hasWriteGrant(returned)) {
        invoke.reject("Folder write permission was not persisted")
        return
      }
      invoke.resolve(JSObject().apply {
        put("selected", true)
        put("treeUri", returned.toString())
        put("writeGranted", true)
      })
    } catch (_: Exception) {
      invoke.reject("Could not persist selected folder write access")
    } finally {
      pickerActive.set(false)
    }
  }

  @ActivityCallback
  private fun treeSelected(invoke: Invoke, result: ActivityResult) {
    try {
      if (result.resultCode != Activity.RESULT_OK) {
        invoke.resolve(JSObject().apply { put("selected", false); put("cancelled", true) })
        return
      }
      val data = result.data
      val uri = data?.data
      if (uri == null || !canonicalSelectedTree(uri) ||
        (data.flags and Intent.FLAG_GRANT_READ_URI_PERMISSION) == 0) {
        invoke.reject("Picker did not return a readable document tree")
        return
      }
      val previous = storedUri()
      activity.contentResolver.takePersistableUriPermission(
        uri, Intent.FLAG_GRANT_READ_URI_PERMISSION
      )
      if (!hasReadGrant(uri)) {
        invoke.reject("Document tree read access was not persisted")
        return
      }
      val saved = activity.getSharedPreferences(PREFS, Activity.MODE_PRIVATE)
        .edit().putString(TREE_KEY, uri.toString()).commit()
      if (!saved) {
        if (previous != uri) releaseOwnGrant(uri)
        invoke.reject("Could not save selected document-tree access")
        return
      }
      if (previous != null && previous != uri) releaseOwnGrant(previous)
      invoke.resolve(JSObject().apply {
        put("selected", true)
        put("granted", true)
        put("treeUri", uri.toString())
      })
    } catch (_: Exception) {
      invoke.reject("Could not persist Android document-tree access")
    } finally {
      pickerActive.set(false)
    }
  }

  private fun releaseOwnGrant(uri: Uri) {
    if (treeStillReferenced(uri)) return
    try {
      activity.contentResolver.releasePersistableUriPermission(
        uri, Intent.FLAG_GRANT_READ_URI_PERMISSION
      )
    } catch (_: Exception) {
      // The former grant may already have been revoked by Android.
    }
  }

  private fun validSegment(name: String): Boolean =
    name.isNotEmpty() && name.length <= MAX_NAME_LENGTH &&
      name != "." && name != ".." &&
      '/' !in name && '\\' !in name && name.none { Character.isISOControl(it) }

  private fun validNewTextName(name: String): Boolean =
    validRenameName(name) && name.endsWith(".txt", ignoreCase = true) &&
      name.length > 4

  /** Create-only names are deliberately stricter than existing SAF path names. */
  private fun validNewFolderName(name: String): Boolean =
    validSegment(name) && validUnicode(name) && name == name.trim() &&
      !name.startsWith(".") && !name.endsWith(".") &&
      Normalizer.isNormalized(name, Normalizer.Form.NFC) &&
      name.none { it in "<>:\"|?*" || Character.getType(it) == Character.FORMAT.toInt() }

  /** Reject malformed Unicode rather than silently replacing it during UTF-8 encoding. */
  private fun validUnicode(text: String): Boolean {
    var i = 0
    while (i < text.length) {
      val ch = text[i]
      if (Character.isHighSurrogate(ch)) {
        if (i + 1 >= text.length || !Character.isLowSurrogate(text[i + 1])) return false
        i++
      } else if (Character.isLowSurrogate(ch)) {
        return false
      }
      i++
    }
    return true
  }

  private fun parseArgs(invoke: Invoke, requireFile: Boolean): DocumentTreeArgs? {
    return try {
      val args = invoke.parseArgs(DocumentTreeArgs::class.java)
      if (args.treeUri.length > 8192 || args.path.size > MAX_DEPTH ||
        (requireFile && args.path.isEmpty()) || !args.path.all(::validSegment)) {
        invoke.reject("Invalid document-tree path")
        null
      } else {
        args
      }
    } catch (_: Exception) {
      invoke.reject("Invalid document-tree arguments")
      null
    }
  }

  private fun selectedUri(raw: String): Uri {
    val active = activeTree() ?: throw SecurityException("No selected tree")
    if (raw != active.toString()) throw SecurityException("Tree does not match selection")
    return active
  }

  private fun children(tree: Uri, parentId: String): List<Entry> {
    if (parentId.length !in 1..MAX_DOCUMENT_ID_LENGTH)
      throw IOException("Invalid parent document ID")
    val resolver = activity.contentResolver
    val queryUri = DocumentsContract.buildChildDocumentsUriUsingTree(tree, parentId)
    val projection = arrayOf(
      DocumentsContract.Document.COLUMN_DOCUMENT_ID,
      DocumentsContract.Document.COLUMN_DISPLAY_NAME,
      DocumentsContract.Document.COLUMN_MIME_TYPE,
      DocumentsContract.Document.COLUMN_SIZE
    )
    val entries = ArrayList<Entry>()
    val cursor = resolver.query(queryUri, projection, null, null, null)
      ?: throw IOException("Document provider did not return children")
    cursor.use {
      while (it.moveToNext()) {
        if (entries.size >= MAX_ENTRIES) throw IOException("Too many document entries")
        val id = it.getString(0) ?: throw IOException("Missing document ID")
        val name = it.getString(1) ?: throw IOException("Missing document name")
        val mimeType = it.getString(2) ?: throw IOException("Missing MIME type")
        if (id.isEmpty() || id.length > MAX_DOCUMENT_ID_LENGTH ||
          !validSegment(name) || mimeType.length > 256 || mimeType.isBlank()) {
          throw IOException("Invalid document provider entry")
        }
        val size = if (it.isNull(3)) -1L else it.getLong(3)
        if (size < -1L || size > MAX_SAFE_NUMBER)
          throw IOException("Invalid provider child document size")
        entries.add(Entry(id, name, mimeType, size))
      }
    }
    return entries
  }

  /** Resolve strictly through listings under the selected tree, never from a caller's document ID. */
  private fun resolvePath(tree: Uri, path: List<String>): Entry {
    val rootId = DocumentsContract.getTreeDocumentId(tree)
    if (rootId.length !in 1..MAX_DOCUMENT_ID_LENGTH)
      throw IOException("Selected tree root ID is invalid")
    var current = Entry(rootId, "", DIRECTORY, -1)
    for (segment in path) {
      if (!current.isDirectory) throw IOException("Document path is not a directory")
      val matches = children(tree, current.documentId).filter { it.name == segment }
      if (matches.size != 1) throw IOException("Document path is missing or ambiguous")
      current = matches.single()
    }
    return current
  }

  /** SAF supports directory creation only when the provider advertises it. */
  private fun directoryAllowsCreate(tree: Uri, directoryId: String): Boolean {
    if (directoryId.length !in 1..MAX_DOCUMENT_ID_LENGTH) return false
    val uri = DocumentsContract.buildDocumentUriUsingTree(tree, directoryId)
    val fields = arrayOf(
      DocumentsContract.Document.COLUMN_DOCUMENT_ID,
      DocumentsContract.Document.COLUMN_MIME_TYPE,
      DocumentsContract.Document.COLUMN_FLAGS
    )
    val cursor = activity.contentResolver.query(uri, fields, null, null, null)
      ?: return false
    return cursor.use {
      it.moveToFirst() && !it.isNull(0) && !it.isNull(1) && !it.isNull(2) &&
        it.getString(0) == directoryId && it.getString(1) == DIRECTORY &&
        (it.getInt(2) and DocumentsContract.Document.FLAG_DIR_SUPPORTS_CREATE) != 0 &&
        !it.moveToNext()
    }
  }

  private fun selectedWriteTree(raw: String): Uri {
    val tree = selectedUri(raw)
    if (!hasWriteGrant(tree)) throw SecurityException("Write permission was not granted")
    return tree
  }

  private fun visibleForFolderConfirmation(): Boolean =
    !activity.isFinishing && !activity.isDestroyed &&
      (activity as? LifecycleOwner)?.lifecycle?.currentState
        ?.isAtLeast(Lifecycle.State.RESUMED) == true

  private class FolderCreationFailure(val publicMessage: String) : IOException(publicMessage)

  /**
   * The only mutation is DocumentsContract.createDocument with the directory
   * MIME type. We never delete a created object on verification failure: an
   * inconsistent/racing provider may have created a directory that requires
   * the user's manual inspection. No existing IDs are passed to a write API.
   */
  private fun createNewFolder(args: DocumentTreeCreateFolderArgs): JSObject {
    var providerCalled = false
    try {
      if (!visibleForFolderConfirmation())
        throw FolderCreationFailure("Folder creation cancelled: app is no longer foreground")
      val tree = selectedWriteTree(args.treeUri)
      val parent = resolvePath(tree, args.path)
      if (!parent.isDirectory || !directoryAllowsCreate(tree, parent.documentId))
        throw FolderCreationFailure("Selected parent cannot create child folders")
      val before = children(tree, parent.documentId)
      if (before.size >= MAX_ENTRIES)
        throw FolderCreationFailure("Parent has reached the 128-entry safety limit")
      if (before.any { sameFolderName(it.name, args.name) })
        throw FolderCreationFailure("A file or folder with that name already exists")
      val beforeIds = before.mapTo(HashSet()) { it.documentId }
      if (beforeIds.size != before.size)
        throw FolderCreationFailure("Parent has ambiguous provider document IDs")

      // Recheck parent ID and grants immediately before the sole provider write.
      if (!visibleForFolderConfirmation() || selectedWriteTree(args.treeUri) != tree)
        throw FolderCreationFailure("Selected tree is no longer available for writing")
      val freshParent = resolvePath(tree, args.path)
      if (!freshParent.isDirectory || freshParent.documentId != parent.documentId ||
        !directoryAllowsCreate(tree, parent.documentId))
        throw FolderCreationFailure("Selected parent changed before creation")
      val freshChildren = children(tree, parent.documentId)
      if (freshChildren.size >= MAX_ENTRIES ||
        freshChildren.any { sameFolderName(it.name, args.name) } ||
        !sameChildIdentities(before, freshChildren))
        throw FolderCreationFailure("Parent contents changed before creation")

      val parentUri = DocumentsContract.buildDocumentUriUsingTree(tree, parent.documentId)
      providerCalled = true
      val newUri = DocumentsContract.createDocument(
        activity.contentResolver, parentUri, DIRECTORY, args.name
      ) ?: throw IOException("Provider declined directory creation")
      if (newUri.scheme != "content" || newUri.authority != tree.authority ||
        !DocumentsContract.isDocumentUri(activity, newUri))
        throw IOException("Provider returned an unexpected document URI")
      val newId = DocumentsContract.getDocumentId(newUri)
      if (newId.isEmpty() || newId.length > MAX_DOCUMENT_ID_LENGTH || newId in beforeIds ||
        newId == parent.documentId ||
        newUri != DocumentsContract.buildDocumentUriUsingTree(tree, newId))
        throw IOException("Provider returned an invalid or existing document ID")

      // Direct child listing is the parent-membership proof. The child document
      // metadata must independently confirm the returned ID/name/MIME type.
      val after = children(tree, parent.documentId)
      val afterIds = after.mapTo(HashSet()) { it.documentId }
      if (after.size != before.size + 1 ||
        afterIds.size != after.size || !afterIds.containsAll(beforeIds) ||
        !sameChildIdentities(before, after.filter { it.documentId in beforeIds }) ||
        after.count { it.documentId == newId } != 1 ||
        after.count { sameFolderName(it.name, args.name) } != 1 ||
        after.single { it.documentId == newId }.let {
          it.name != args.name || it.mimeType != DIRECTORY
        }) throw IOException("Provider could not verify exact new child directory")
      val confirmedUri = DocumentsContract.buildDocumentUriUsingTree(tree, newId)
      val fields = arrayOf(DocumentsContract.Document.COLUMN_DOCUMENT_ID,
        DocumentsContract.Document.COLUMN_DISPLAY_NAME,
        DocumentsContract.Document.COLUMN_MIME_TYPE)
      val cursor = activity.contentResolver.query(confirmedUri, fields, null, null, null)
        ?: throw IOException("Provider did not return new directory metadata")
      val metadataMatches = cursor.use {
        it.moveToFirst() && it.getString(0) == newId &&
          it.getString(1) == args.name && it.getString(2) == DIRECTORY && !it.moveToNext()
      }
      if (!metadataMatches) throw IOException("Provider directory metadata mismatch")
      if (!visibleForFolderConfirmation() || selectedWriteTree(args.treeUri) != tree ||
        resolvePath(tree, args.path).documentId != parent.documentId)
        throw IOException("Selected tree, parent, or write grant changed after creation")

      return JSObject().apply {
        put("created", true)
        put("treeUri", tree.toString())
        put("path", JSONArray(args.path + args.name))
        put("parentPath", JSONArray(args.path))
        put("name", args.name)
        put("isDirectory", true)
        put("mimeType", DIRECTORY)
      }
    } catch (error: FolderCreationFailure) {
      if (providerCalled) throw FolderCreationFailure(
        "Folder creation could not be verified; a new folder may remain in the selected parent")
      throw error
    } catch (_: SecurityException) {
      throw FolderCreationFailure(if (providerCalled)
        "Folder created or attempted, but SAF permission was revoked; a folder may remain"
      else "Selected tree no longer has persisted read and write access")
    } catch (_: Exception) {
      throw FolderCreationFailure(if (providerCalled)
        "Folder creation could not be verified; a new folder may remain in the selected parent"
      else "Document provider could not verify the selected parent")
    }
  }

  private fun sameFolderName(existing: String, requested: String): Boolean =
    Normalizer.normalize(existing, Normalizer.Form.NFC).equals(requested, ignoreCase = true)

  /** Ignore changing file sizes but never tolerate an old child ID acquiring
   * a different display name or MIME type between provider calls. */
  private fun sameChildIdentities(before: List<Entry>, after: List<Entry>): Boolean =
    before.size == after.size &&
      before.mapTo(HashSet()) { it.documentId }.size == before.size &&
      after.mapTo(HashSet()) { it.documentId }.size == after.size &&
      before.all { original -> after.count {
        it.documentId == original.documentId &&
          it.name == original.name && it.mimeType == original.mimeType
      } == 1 }

  /** Check full Unicode code points, including supplementary format controls
   * that a UTF-16 Char-by-Char check would otherwise miss. */
  private fun validRenameName(name: String): Boolean =
    validNewFolderName(name) && name.codePoints().allMatch { codePoint ->
      val category = Character.getType(codePoint)
      val noncharacter = codePoint in 0xFDD0..0xFDEF ||
        (codePoint and 0xFFFE) == 0xFFFE
      !noncharacter && category != Character.FORMAT.toInt() &&
        category != Character.PRIVATE_USE.toInt() &&
        category != Character.UNASSIGNED.toInt() &&
        category != Character.SURROGATE.toInt()
    }

  /** A successful property report is based only on bounded DocumentsProvider
   * fields. The flags are capabilities advertised by the provider, NOT a
   * promise of current WRITE/RENAME/DELETE permission. */
  private data class PropertyMetadata(
    val documentId: String,
    val name: String,
    val mimeType: String,
    val size: Long?,
    val lastModified: Long?,
    val flags: Int
  ) {
    val isDirectory: Boolean get() = mimeType == DIRECTORY
  }

  private data class PropertyWalk(
    val ancestorIds: List<String>,
    val metadata: PropertyMetadata
  )

  /** Query exactly one canonical document URI, not a caller-controlled URI.
   * Reject extra rows, missing IDs, hidden/invalid display names, malformed
   * size/timestamp fields, and oversized metadata before emitting anything. */
  private fun propertyMetadata(tree: Uri, documentId: String): PropertyMetadata {
    if (documentId.length !in 1..MAX_DOCUMENT_ID_LENGTH)
      throw IOException("Document provider ID is invalid")
    val uri = DocumentsContract.buildDocumentUriUsingTree(tree, documentId)
    if (uri.scheme != "content" || uri.authority != tree.authority ||
      !DocumentsContract.isDocumentUri(activity, uri) ||
      DocumentsContract.getDocumentId(uri) != documentId)
      throw IOException("Document provider URI is not canonical")
    val fields = arrayOf(
      DocumentsContract.Document.COLUMN_DOCUMENT_ID,
      DocumentsContract.Document.COLUMN_DISPLAY_NAME,
      DocumentsContract.Document.COLUMN_MIME_TYPE,
      DocumentsContract.Document.COLUMN_SIZE,
      DocumentsContract.Document.COLUMN_LAST_MODIFIED,
      DocumentsContract.Document.COLUMN_FLAGS
    )
    val cursor = activity.contentResolver.query(uri, fields, null, null, null)
      ?: throw IOException("Provider metadata is not available")
    return cursor.use {
      if (!it.moveToFirst() || (0..2).any(it::isNull) || it.isNull(5))
        throw IOException("Required document metadata is missing")
      val id = it.getString(0)
      val name = it.getString(1)
      val mime = it.getString(2)
      if (id != documentId || !validRenameName(name) ||
        mime.length !in 1..256 || mime != mime.trim() ||
        '/' !in mime || mime.any { ch -> Character.isISOControl(ch) ||
          Character.getType(ch) == Character.FORMAT.toInt() })
        throw IOException("Document provider returned unsafe metadata")
      val size = if (it.isNull(3)) null else it.getLong(3).also { value ->
        if (value < -1L || value > MAX_SAFE_NUMBER)
          throw IOException("Invalid provider document size")
      }.takeUnless { value -> value == -1L }
      val modified = if (it.isNull(4)) null else it.getLong(4).also { value ->
        if (value < 0 || value > MAX_PROVIDER_TIMESTAMP_MS)
          throw IOException("Invalid provider modification timestamp")
      }.takeUnless { value -> value == 0L }
      val flags = it.getInt(5)
      if (flags < 0 || it.moveToNext())
        throw IOException("Provider returned invalid document flags or extra rows")
      PropertyMetadata(id, name, mime, size, modified, flags)
    }
  }

  /** Every child ID is derived from a direct parent listing; IDs from callers
   * or document URIs outside the selected tree can never enter this walk. */
  private fun propertyWalk(tree: Uri, path: List<String>): PropertyWalk {
    val rootId = DocumentsContract.getTreeDocumentId(tree)
    if (rootId.length !in 1..MAX_DOCUMENT_ID_LENGTH)
      throw IOException("Selected tree root ID is invalid")
    var current = propertyMetadata(tree, rootId)
    val ancestry = ArrayList<String>(path.size + 1)
    ancestry.add(rootId)
    for (segment in path) {
      if (!current.isDirectory) throw IOException("Path crosses a non-directory")
      val siblings = children(tree, current.documentId)
      // Duplicate IDs or names that differ only in case/normalization are not
      // safe to use for a user-facing property lookup.
      if (siblings.mapTo(HashSet()) { it.documentId }.size != siblings.size)
        throw IOException("Provider child IDs are ambiguous")
      val matches = siblings.filter { sameFolderName(it.name, segment) }
      if (matches.size != 1 || matches.single().name != segment)
        throw IOException("Selected child name is missing or ambiguous")
      val entry = matches.single()
      if (!validRenameName(entry.name) || entry.documentId in ancestry)
        throw IOException("Provider child name or ancestry is invalid")
      val next = propertyMetadata(tree, entry.documentId)
      if (next.name != entry.name || next.mimeType != entry.mimeType ||
        (entry.size >= 0L && next.size != entry.size))
        throw IOException("Provider child metadata differs from parent listing")
      ancestry.add(entry.documentId)
      current = next
    }
    return PropertyWalk(ancestry, current)
  }

  @Command
  fun properties(invoke: Invoke) {
    val args = try { invoke.parseArgs(DocumentTreePropertiesArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid document properties arguments"); return }
    val valid = try {
      args.userInitiated && args.treeUri.length in 1..8192 &&
        args.path.size <= MAX_DEPTH && args.path.all(::validRenameName)
    } catch (_: Exception) { false }
    if (!valid) {
      invoke.reject("Document properties require explicit action and a safe selected-tree path")
      return
    }
    submit(invoke) {
      // No content read, no writes, no permission picker, and no broad storage
      // requests. A persisted READ grant to this exact selected tree suffices.
      val tree = selectedUri(args.treeUri)
      if (tree.scheme != "content" || !DocumentsContract.isTreeUri(tree))
        throw SecurityException("Selected tree is not a canonical SAF URI")
      val initial = propertyWalk(tree, args.path)
      if (selectedUri(args.treeUri) != tree)
        throw SecurityException("Selected tree was revoked or replaced")
      val current = propertyWalk(tree, args.path)
      if (initial != current || selectedUri(args.treeUri) != tree)
        throw IOException("Provider metadata or document ancestry changed")
      val metadata = current.metadata
      JSObject().apply {
        put("treeUri", tree.toString())
        put("path", JSONArray(args.path))
        put("name", metadata.name)
        put("isDirectory", metadata.isDirectory)
        put("mimeType", metadata.mimeType)
        if (metadata.size != null) put("size", metadata.size)
        if (metadata.lastModified != null) put("lastModified", metadata.lastModified)
        // Provider capability bits. No claim of actual user WRITE access.
        put("canWrite", (metadata.flags and
          DocumentsContract.Document.FLAG_SUPPORTS_WRITE) != 0)
        put("canRename", (metadata.flags and
          DocumentsContract.Document.FLAG_SUPPORTS_RENAME) != 0)
        put("canDelete", (metadata.flags and
          DocumentsContract.Document.FLAG_SUPPORTS_DELETE) != 0)
      }
    }
  }

  private class EntryRenameFailure(val publicMessage: String) : IOException(publicMessage)

  /** A bounded, single-row metadata query that proves the direct child ID and
   * name observed in the parent listing describe the same provider document. */
  private fun documentMetadata(tree: Uri, entry: Entry, requireRename: Boolean): Boolean {
    if (entry.documentId.length !in 1..MAX_DOCUMENT_ID_LENGTH ||
      entry.mimeType.length !in 1..256 || !validSegment(entry.name)) return false
    val documentUri = DocumentsContract.buildDocumentUriUsingTree(tree, entry.documentId)
    val fields = arrayOf(
      DocumentsContract.Document.COLUMN_DOCUMENT_ID,
      DocumentsContract.Document.COLUMN_DISPLAY_NAME,
      DocumentsContract.Document.COLUMN_MIME_TYPE,
      DocumentsContract.Document.COLUMN_FLAGS
    )
    val cursor = activity.contentResolver.query(documentUri, fields, null, null, null)
      ?: return false
    return cursor.use {
      if (!it.moveToFirst() || it.isNull(0) || it.isNull(1) || it.isNull(2)) return@use false
      val matching = it.getString(0) == entry.documentId &&
        it.getString(1) == entry.name && it.getString(2) == entry.mimeType &&
        (!requireRename || (!it.isNull(3) &&
          (it.getInt(3) and DocumentsContract.Document.FLAG_SUPPORTS_RENAME) != 0))
      matching && !it.moveToNext()
    }
  }

  /** Independently verify the selected parent as a real document directory.
   * Includes tree root, for which the synthetic Entry has no display name. */
  private fun parentMetadata(tree: Uri, parentId: String): Boolean {
    if (parentId.length !in 1..MAX_DOCUMENT_ID_LENGTH) return false
    val uri = DocumentsContract.buildDocumentUriUsingTree(tree, parentId)
    if (!DocumentsContract.isDocumentUri(activity, uri) ||
      uri.authority != tree.authority) return false
    val columns = arrayOf(
      DocumentsContract.Document.COLUMN_DOCUMENT_ID,
      DocumentsContract.Document.COLUMN_MIME_TYPE
    )
    val cursor = activity.contentResolver.query(uri, columns, null, null, null)
      ?: return false
    return cursor.use {
      it.moveToFirst() && !it.isNull(0) && !it.isNull(1) &&
        it.getString(0) == parentId && it.getString(1) == DIRECTORY &&
        !it.moveToNext()
    }
  }

  /** Keep non-target siblings byte-for-byte identical in provider metadata.
   * Document IDs may change for the renamed target only. */
  private fun renameVerified(
    before: List<Entry>, after: List<Entry>, old: Entry,
    returnedId: String, requestedName: String
  ): Boolean {
    if (before.size != after.size || before.size !in 1..MAX_ENTRIES ||
      returnedId.length !in 1..MAX_DOCUMENT_ID_LENGTH || returnedId == "") return false
    if (before.count { it.documentId == old.documentId && it.name == old.name } != 1 ||
      before.mapTo(HashSet()) { it.documentId }.size != before.size ||
      after.mapTo(HashSet()) { it.documentId }.size != after.size) return false
    val target = after.singleOrNull { it.documentId == returnedId } ?: return false
    if (target.name != requestedName || target.mimeType != old.mimeType ||
      after.count { sameFolderName(it.name, requestedName) } != 1 ||
      after.any { it.documentId == old.documentId && it.documentId != returnedId }) return false
    // Every other sibling ID, name and MIME must remain in the same parent.
    val siblings = before.filter { it.documentId != old.documentId }
    val remaining = after.filter { it.documentId != returnedId }
    return siblings.size == remaining.size && siblings.all { sibling ->
      remaining.count { it.documentId == sibling.documentId &&
        it.name == sibling.name && it.mimeType == sibling.mimeType } == 1
    }
  }

  /** Never attempts a restore after a provider might have changed an entry.
   * An uncertain outcome is NOT a success receipt and asks users to inspect
   * the chosen parent. It does not include an unverified path as authoritative. */
  private fun renameUncertain(): JSObject = JSObject().apply {
    put("renamed", false)
    put("verified", false)
    put("uncertain", true)
    put("providerAttempted", true)
    put("message", "Provider rename may have changed the entry; inspect the selected parent before retrying")
  }

  private fun renameExistingEntry(args: DocumentTreeRenameEntryArgs): JSObject {
    var providerAttempted = false
    try {
      if (!visibleForFolderConfirmation())
        throw EntryRenameFailure("Rename cancelled because the app is not foreground")
      val tree = selectedWriteTree(args.treeUri)
      val parentPath = args.path.dropLast(1)
      val oldName = args.path.last()
      val parent = resolvePath(tree, parentPath)
      if (!parent.isDirectory || !parentMetadata(tree, parent.documentId))
        throw EntryRenameFailure("Selected parent is not a verified directory")
      val before = children(tree, parent.documentId)
      if (before.mapTo(HashSet()) { it.documentId }.size != before.size)
        throw EntryRenameFailure("Provider parent has duplicate document IDs")
      val matches = before.filter { it.name == oldName }
      if (matches.size != 1)
        throw EntryRenameFailure("Selected entry is missing or ambiguous")
      val old = matches.single()
      if (sameFolderName(old.name, args.name))
        throw EntryRenameFailure("New name must be different from existing name")
      if (before.any { it.documentId != old.documentId && sameFolderName(it.name, args.name) })
        throw EntryRenameFailure("A sibling already uses the requested name")
      if (!documentMetadata(tree, old, requireRename = true))
        throw EntryRenameFailure("Provider has not enabled rename for this entry")

      // Verify same tree, parent ID and exact listing again immediately before
      // invoking the provider. Never use a document ID supplied by the caller.
      if (!visibleForFolderConfirmation() || selectedWriteTree(args.treeUri) != tree)
        throw EntryRenameFailure("Selected tree or write permission changed")
      val freshParent = resolvePath(tree, parentPath)
      if (!freshParent.isDirectory || freshParent.documentId != parent.documentId ||
        !parentMetadata(tree, freshParent.documentId))
        throw EntryRenameFailure("Selected parent changed before rename")
      val fresh = children(tree, parent.documentId)
      if (fresh.size != before.size || !before.all { first ->
        fresh.count { it.documentId == first.documentId &&
          it.name == first.name && it.mimeType == first.mimeType } == 1
      } || !documentMetadata(tree, old, requireRename = true))
        throw EntryRenameFailure("Selected entry or sibling list changed before rename")

      val oldUri = DocumentsContract.buildDocumentUriUsingTree(tree, old.documentId)
      providerAttempted = true
      val renamedUri = DocumentsContract.renameDocument(
        activity.contentResolver, oldUri, args.name
      ) ?: throw IOException("Provider returned no renamed document")
      if (renamedUri.scheme != "content" || renamedUri.authority != tree.authority ||
        !DocumentsContract.isDocumentUri(activity, renamedUri))
        throw IOException("Provider returned a URI outside the selected tree")
      val newId = DocumentsContract.getDocumentId(renamedUri)
      if (newId.length !in 1..MAX_DOCUMENT_ID_LENGTH ||
        newId != old.documentId && before.any { it.documentId == newId } ||
        renamedUri != DocumentsContract.buildDocumentUriUsingTree(tree, newId))
        throw IOException("Provider returned invalid or reused document ID")

      val afterParent = resolvePath(tree, parentPath)
      val after = children(tree, parent.documentId)
      if (!afterParent.isDirectory || afterParent.documentId != parent.documentId ||
        !parentMetadata(tree, afterParent.documentId) ||
        !renameVerified(before, after, old, newId, args.name))
        throw IOException("Provider did not prove the renamed entry remained in the parent")
      val newEntry = after.single { it.documentId == newId }
      if (!documentMetadata(tree, newEntry, requireRename = false) ||
        !visibleForFolderConfirmation() || selectedWriteTree(args.treeUri) != tree ||
        resolvePath(tree, parentPath).documentId != parent.documentId)
        throw IOException("Rename verification or write permission changed")

      return JSObject().apply {
        put("renamed", true)
        put("verified", true)
        put("uncertain", false)
        put("treeUri", tree.toString())
        put("parentPath", JSONArray(parentPath))
        put("oldPath", JSONArray(args.path))
        put("newPath", JSONArray(parentPath + args.name))
        put("oldName", old.name)
        put("name", newEntry.name)
        put("mimeType", newEntry.mimeType)
        put("isDirectory", newEntry.isDirectory)
        put("documentIdChanged", old.documentId != newEntry.documentId)
      }
    } catch (error: EntryRenameFailure) {
      if (providerAttempted) return renameUncertain()
      throw error
    } catch (_: Exception) {
      if (providerAttempted) return renameUncertain()
      throw EntryRenameFailure("Could not verify the selected entry or persisted write access")
    }
  }

  @Command
  fun renameEntry(invoke: Invoke) {
    val args = try { invoke.parseArgs(DocumentTreeRenameEntryArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid rename arguments"); return }
    val valid = try {
      args.userInitiated && args.treeUri.length in 1..8192 &&
        args.path.size in 1..MAX_DEPTH && args.path.all(::validSegment) &&
        validRenameName(args.name)
    } catch (_: Exception) { false }
    if (!valid) {
      invoke.reject("Rename requires an existing entry path, safe new name and explicit user action")
      return
    }
    val tree = activeTree()
    if (tree == null || tree.toString() != args.treeUri || !hasWriteGrant(tree)) {
      invoke.reject("Rename requires persisted READ and separate WRITE access to the same selected tree")
      return
    }
    if (!visibleForFolderConfirmation()) {
      invoke.reject("Rename requires the app to be foreground")
      return
    }
    if (!renameConfirmationActive.compareAndSet(false, true)) {
      invoke.reject("Another rename is already pending")
      return
    }
    activity.runOnUiThread {
      try {
        if (!visibleForFolderConfirmation()) throw SecurityException("App not foreground")
        // Show the exact existing entry, destination sibling name and tree-relative parent.
        val parent = if (args.path.size == 1) "the selected tree root"
          else "selected tree / " + args.path.dropLast(1).joinToString(" / ")
        AlertDialog.Builder(activity)
          .setTitle("Rename selected item?")
          .setMessage("Rename “${args.path.last()}” to “${args.name}” in $parent?\n" +
            "Only this existing file or folder will be renamed; siblings cannot be overwritten.")
          .setPositiveButton("Rename") { _, _ ->
            try {
              worker.execute {
                try { invoke.resolve(renameExistingEntry(args)) }
                catch (error: EntryRenameFailure) { invoke.reject(error.publicMessage) }
                finally { renameConfirmationActive.set(false) }
              }
            } catch (_: Exception) {
              renameConfirmationActive.set(false)
              invoke.reject("Document-tree operation queue is full")
            }
          }
          .setNegativeButton("Cancel") { _, _ ->
            renameConfirmationActive.set(false)
            invoke.resolve(JSObject().apply {
              put("renamed", false)
              put("cancelled", true)
              put("uncertain", false)
            })
          }
          .setOnCancelListener {
            renameConfirmationActive.set(false)
            invoke.resolve(JSObject().apply {
              put("renamed", false)
              put("cancelled", true)
              put("uncertain", false)
            })
          }
          .show()
      } catch (_: Exception) {
        renameConfirmationActive.set(false)
        invoke.reject("Could not display Android rename confirmation")
      }
    }
  }

  /**
   * The native confirmation is required in addition to a UI gesture and the
   * separate persisted same-tree WRITE grant. One pending folder operation at
   * a time, including dialog and provider verification.
   */
  @Command
  fun createFolder(invoke: Invoke) {
    val args = try { invoke.parseArgs(DocumentTreeCreateFolderArgs::class.java) }
      catch (_: Exception) { invoke.reject("Invalid create-folder arguments"); return }
    val valid = try {
      args.userInitiated && args.treeUri.length in 1..8192 &&
        args.path.size < MAX_DEPTH && args.path.all(::validSegment) &&
        validNewFolderName(args.name)
    } catch (_: Exception) { false }
    if (!valid) {
      invoke.reject("Create folder requires an explicit action, safe name, and bounded parent path")
      return
    }
    if (activeTree()?.toString() != args.treeUri ||
      !hasWriteGrant(activeTree() ?: run {
        invoke.reject("No selected tree has persisted read access")
        return
      })) {
      invoke.reject("Selected tree has no separate persisted write permission")
      return
    }
    if (!visibleForFolderConfirmation()) {
      invoke.reject("Open the app before creating a folder")
      return
    }
    if (!folderCreationActive.compareAndSet(false, true)) {
      invoke.reject("Another folder creation is already pending")
      return
    }
    activity.runOnUiThread {
      try {
        if (!visibleForFolderConfirmation())
          throw SecurityException("Activity left foreground")
        val location = if (args.path.isEmpty()) "selected tree root"
          else "selected tree / " + args.path.joinToString(" / ")
        AlertDialog.Builder(activity)
          .setTitle("Create a new folder?")
          .setMessage("Create a NEW folder named “${args.name}” in $location?\n" +
            "Existing files and folders will not be changed or replaced.")
          .setPositiveButton("Create") { _, _ ->
            try {
              worker.execute {
                try { invoke.resolve(createNewFolder(args)) }
                catch (error: FolderCreationFailure) { invoke.reject(error.publicMessage) }
                finally { folderCreationActive.set(false) }
              }
            } catch (_: Exception) {
              folderCreationActive.set(false)
              invoke.reject("Document-tree operation queue is full")
            }
          }
          .setNegativeButton("Cancel") { _, _ ->
            folderCreationActive.set(false)
            invoke.resolve(JSObject().apply { put("created", false); put("cancelled", true) })
          }
          .setOnCancelListener {
            folderCreationActive.set(false)
            invoke.resolve(JSObject().apply { put("created", false); put("cancelled", true) })
          }
          .show()
      } catch (_: Exception) {
        folderCreationActive.set(false)
        invoke.reject("Could not show Android create-folder confirmation")
      }
    }
  }

  /**
   * Creates only a new text/plain file. No existing URI is ever opened for
   * output; no overwrite, rename, delete, or arbitrary filesystem path APIs.
   *
   * SAF providers can race another app, auto-adjust names, or fail after
   * creating an empty document. In those cases we refuse to write, without
   * deleting/renaming anything; the user may need to remove an empty file.
   */
  private fun createNewTextFile(args: DocumentTreeCreateTextArgs, bytes: ByteArray): JSObject {
    if (!visibleForFolderConfirmation())
      throw IOException("File creation requires foreground Activity")
    val tree = selectedWriteTree(args.treeUri)
    val directory = resolvePath(tree, args.path)
    if (!directory.isDirectory || !parentMetadata(tree, directory.documentId) ||
      !directoryAllowsCreate(tree, directory.documentId))
      throw IOException("Directory cannot create documents")
    val before = children(tree, directory.documentId)
    if (before.size >= MAX_ENTRIES ||
      before.any { sameFolderName(it.name, args.name) }) {
      throw IOException("Name already exists or folder entry limit reached")
    }
    val existingIds = before.mapTo(HashSet()) { it.documentId }
    if (existingIds.size != before.size)
      throw IOException("Provider parent has ambiguous document IDs")
    // Recheck user-selected tree and separate persisted write grant immediately
    // before invoking the SAF provider.
    if (selectedWriteTree(args.treeUri) != tree || !visibleForFolderConfirmation())
      throw IOException("Selected tree or Activity is no longer available")
    val freshDirectory = resolvePath(tree, args.path)
    if (!freshDirectory.isDirectory ||
      freshDirectory.documentId != directory.documentId ||
      !parentMetadata(tree, directory.documentId) ||
      !directoryAllowsCreate(tree, directory.documentId))
      throw IOException("Selected parent changed before text creation")
    val freshChildren = children(tree, directory.documentId)
    if (freshChildren.size >= MAX_ENTRIES ||
      !sameChildIdentities(before, freshChildren) ||
      freshChildren.any { sameFolderName(it.name, args.name) })
      throw IOException("Parent contents changed before text creation")
    val parentUri = DocumentsContract.buildDocumentUriUsingTree(tree, directory.documentId)
    val created = DocumentsContract.createDocument(
      activity.contentResolver, parentUri, "text/plain", args.name
    ) ?: throw IOException("Document creation was declined by provider")
    if (created.scheme != "content" || created.authority != tree.authority ||
      !DocumentsContract.isDocumentUri(activity, created))
      throw IOException("Provider returned an unexpected document URI")
    val createdId = DocumentsContract.getDocumentId(created)
    if (createdId.isEmpty() || createdId.length > MAX_DOCUMENT_ID_LENGTH ||
      createdId == directory.documentId || createdId in existingIds ||
      created != DocumentsContract.buildDocumentUriUsingTree(tree, createdId)) {
      throw IOException("Provider returned a previously existing document")
    }
    // Confirm the exact new document (name, MIME and parent) through SAF's own
    // child listing before any data is sent. Never follow a provider's rename.
    val after = children(tree, directory.documentId)
    val matching = after.filter { it.documentId == createdId }
    if (after.size != before.size + 1 ||
      after.mapTo(HashSet()) { it.documentId }.size != after.size ||
      !sameChildIdentities(before, after.filter { it.documentId in existingIds }) ||
      after.count { sameFolderName(it.name, args.name) } != 1 ||
      matching.size != 1 || matching[0].name != args.name ||
      matching[0].mimeType != "text/plain" ||
      !documentMetadata(tree, matching[0], requireRename = false) ||
      !parentMetadata(tree, directory.documentId)) {
      throw IOException("Provider adjusted or could not verify new document")
    }
    if (!visibleForFolderConfirmation() || selectedWriteTree(args.treeUri) != tree ||
      resolvePath(tree, args.path).documentId != directory.documentId)
      throw IOException("Parent or tree permission changed before writing")
    // 'w' writes only the freshly-created document URI whose ID was absent
    // before creation. Never pass an existing user-controlled URI here.
    val output = activity.contentResolver.openOutputStream(created, "w")
      ?: throw IOException("Provider did not open newly created document for writing")
    output.use {
      it.write(bytes)
      it.flush()
    }
    // Successful stream.close() alone is not proof that the provider preserved
    // this object's parent/name. Never return success on ambiguous post-state.
    val finalChildren = children(tree, directory.documentId)
    if (finalChildren.size != before.size + 1 ||
      finalChildren.mapTo(HashSet()) { it.documentId }.size != finalChildren.size ||
      !sameChildIdentities(before, finalChildren.filter { it.documentId in existingIds }) ||
      finalChildren.count { sameFolderName(it.name, args.name) } != 1 ||
      finalChildren.count { it.documentId == createdId && it.name == args.name &&
        it.mimeType == "text/plain" } != 1 ||
      !documentMetadata(tree, matching[0], requireRename = false) ||
      resolvePath(tree, args.path).documentId != directory.documentId ||
      selectedWriteTree(args.treeUri) != tree)
      throw IOException("New document's provider identity changed after writing")
    return JSObject().apply {
      put("created", true)
      put("treeUri", tree.toString())
      put("path", JSONArray(args.path))
      put("name", args.name)
      put("mimeType", "text/plain")
      put("encoding", "utf-8")
      put("size", bytes.size)
    }
  }

  @Command
  fun createText(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(DocumentTreeCreateTextArgs::class.java)
    } catch (_: Exception) {
      invoke.reject("Invalid text-file creation arguments")
      return
    }
    val bytes = try {
      if (!args.userInitiated || args.treeUri.length !in 1..8192 ||
        args.path.size >= MAX_DEPTH || !args.path.all(::validSegment) ||
        !validNewTextName(args.name) || !validUnicode(args.content) ||
        args.content.length > MAX_CREATE_BYTES) {
        invoke.reject("Invalid text-file name, content, path or user action")
        return
      }
      args.content.toByteArray(Charsets.UTF_8).also {
        if (it.size > MAX_CREATE_BYTES) {
          invoke.reject("UTF-8 file content exceeds 64 KiB")
          return
        }
      }
    } catch (_: Exception) {
      invoke.reject("Invalid UTF-8 text creation request")
      return
    }
    if (activeTree()?.toString() != args.treeUri ||
      !hasWriteGrant(activeTree() ?: run {
        invoke.reject("Selected tree is unavailable")
        return
      })) {
      invoke.reject("Selected folder has no persisted write grant")
      return
    }
    if (!visibleForFolderConfirmation()) {
      invoke.reject("Text creation requires the app to be foreground")
      return
    }
    if (!writeConfirmationActive.compareAndSet(false, true)) {
      invoke.reject("Another text-file confirmation is open")
      return
    }
    activity.runOnUiThread {
      try {
        val description = args.path.joinToString("/") { it }
        AlertDialog.Builder(activity)
          .setTitle("Create a new text file?")
          .setMessage("Create ${args.name} (${bytes.size} UTF-8 bytes) in the selected folder" +
            (if (description.isEmpty()) "?" else "/$description?") +
            "\nExisting files will not be overwritten.")
          .setPositiveButton("Create") { _, _ ->
            try {
              worker.execute {
                try {
                  invoke.resolve(createNewTextFile(args, bytes))
                } catch (_: Exception) {
                  invoke.reject(
                    "Could not verify text creation; a new file with empty or partial content may remain"
                  )
                } finally {
                  writeConfirmationActive.set(false)
                }
              }
            } catch (_: Exception) {
              writeConfirmationActive.set(false)
              invoke.reject("Too many pending document-tree operations")
            }
          }
          .setNegativeButton("Cancel") { _, _ ->
            writeConfirmationActive.set(false)
            invoke.resolve(JSObject().apply {
              put("created", false)
              put("cancelled", true)
            })
          }
          .setOnCancelListener {
            writeConfirmationActive.set(false)
            invoke.resolve(JSObject().apply {
              put("created", false)
              put("cancelled", true)
            })
          }
          .show()
      } catch (_: Exception) {
        writeConfirmationActive.set(false)
        invoke.reject("Could not request confirmation for text creation")
      }
    }
  }

  private fun submit(invoke: Invoke, job: () -> JSObject) {
    try {
      worker.execute {
        try {
          invoke.resolve(job())
        } catch (_: SecurityException) {
          invoke.reject("Document-tree read permission is unavailable")
        } catch (_: Exception) {
          // Do not expose provider error strings, filenames, URIs, or file contents.
          invoke.reject("Document-tree operation failed or exceeded its limits")
        }
      }
    } catch (_: Exception) {
      invoke.reject("Too many pending document-tree operations")
    }
  }

  @Command
  fun list(invoke: Invoke) {
    val args = parseArgs(invoke, requireFile = false) ?: return
    submit(invoke) {
      val tree = selectedUri(args.treeUri)
      val directory = resolvePath(tree, args.path)
      if (!directory.isDirectory) throw IOException("Not a directory")
      val result = JSONArray()
      for (entry in children(tree, directory.documentId)) {
        result.put(JSONObject().apply {
          put("name", entry.name)
          put("mimeType", entry.mimeType)
          put("isDirectory", entry.isDirectory)
          put("size", if (entry.size >= 0) entry.size else JSONObject.NULL)
        })
      }
      JSObject().apply {
        put("treeUri", tree.toString())
        put("path", JSONArray(args.path))
        put("entries", result)
      }
    }
  }

  @Command
  fun openDocument(invoke: Invoke) {
    val args = parseArgs(invoke, requireFile = true) ?: return
    submit(invoke) {
      val tree = selectedUri(args.treeUri)
      val entry = resolvePath(tree, args.path)
      if (entry.isDirectory || entry.size > MAX_DOCUMENT_BYTES) {
        throw IOException("Cannot read directory or oversized document")
      }
      val uri = DocumentsContract.buildDocumentUriUsingTree(tree, entry.documentId)
      val output = ByteArrayOutputStream()
      val buffer = ByteArray(8192)
      val stream = activity.contentResolver.openInputStream(uri)
        ?: throw IOException("Unable to open document")
      stream.use { input ->
        while (true) {
          val length = input.read(buffer)
          if (length < 0) break
          if (output.size() + length > MAX_DOCUMENT_BYTES) {
            throw IOException("Document exceeds read limit")
          }
          output.write(buffer, 0, length)
        }
      }
      JSObject().apply {
        put("treeUri", tree.toString())
        put("path", JSONArray(args.path))
        put("mimeType", entry.mimeType)
        put("size", output.size())
        put("encoding", "base64")
        put("data", Base64.encodeToString(output.toByteArray(), Base64.NO_WRAP))
      }
    }
  }
}
